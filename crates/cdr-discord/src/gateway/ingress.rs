#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::{
    sync::{broadcast, mpsc},
    time::Instant,
};
use twilight_model::gateway::{
    event::Event,
    payload::incoming::{InteractionCreate, MessageCreate},
};

use super::{
    GatewayIdentityConflictReceiver, GatewayIdentityReceiver,
    gateway_identity::GatewayIdentityTracker,
};

mod gaps;
mod publication;
mod receive_error;
mod sequence;
#[cfg(test)]
mod test_support;
mod types;

use sequence::SequenceSource;
pub use {gaps::*, types::*};
const DIAGNOSTIC_NOTIFICATION_CAPACITY: usize = 16;

#[derive(Clone, Debug)]
pub struct GatewayIngress {
    topology: super::topology::GatewayTopology,
    normal_interactions: mpsc::Sender<InteractionIngress>,
    reserved_interactions: mpsc::Sender<InteractionIngress>,
    messages: mpsc::Sender<MessageIngress>,
    emergency_messages: mpsc::Sender<MessageIngress>,
    receive_errors: mpsc::Sender<ReceiveErrorIngress>,
    identity: GatewayIdentityTracker,
    message_gaps: gaps::MessageGapTracker,
    diagnostics: Arc<types::IngressDiagnosticState>,
    diagnostic_notifications: broadcast::Sender<()>,
    accepting: Arc<AtomicBool>,
    sequence: SequenceSource,
}

impl GatewayIngress {
    pub fn new(
        config: GatewayIngressConfig,
    ) -> Result<(Self, GatewayIngressReceivers), GatewayIngressConfigError> {
        Self::build(config, SequenceSource::Process)
    }

    #[cfg(test)]
    pub(super) fn new_for_test(
        config: GatewayIngressConfig,
        last_sequence: u64,
    ) -> Result<(Self, GatewayIngressReceivers), GatewayIngressConfigError> {
        Self::build(
            config,
            SequenceSource::Test(Arc::new(AtomicU64::new(last_sequence))),
        )
    }

    fn build(
        config: GatewayIngressConfig,
        sequence: SequenceSource,
    ) -> Result<(Self, GatewayIngressReceivers), GatewayIngressConfigError> {
        config.validate()?;
        let (normal_interactions, normal_rx) = mpsc::channel(config.interaction_capacity);
        let (reserved_interactions, reserved_rx) =
            mpsc::channel(config.reserved_interaction_capacity);
        let (messages, message_rx) = mpsc::channel(config.message_capacity);
        let (emergency_messages, emergency_rx) = mpsc::channel(EMERGENCY_MESSAGE_CAPACITY);
        let (receive_errors, receive_error_rx) = mpsc::channel(config.receive_error_capacity);
        let identity = GatewayIdentityTracker::new();
        let message_gaps = gaps::MessageGapTracker::new();
        let diagnostics = Arc::new(types::IngressDiagnosticState::default());
        let (diagnostic_notifications, _) = broadcast::channel(DIAGNOSTIC_NOTIFICATION_CAPACITY);
        Ok((
            Self {
                topology: super::topology::GatewayTopology::default(),
                normal_interactions,
                reserved_interactions,
                messages,
                emergency_messages,
                receive_errors,
                identity,
                message_gaps,
                diagnostics,
                diagnostic_notifications,
                accepting: Arc::new(AtomicBool::new(true)),
                sequence,
            },
            GatewayIngressReceivers {
                normal_interactions: normal_rx,
                reserved_interactions: reserved_rx,
                messages: message_rx,
                emergency_messages: emergency_rx,
                receive_errors: receive_error_rx,
            },
        ))
    }

    #[must_use]
    pub fn publish(&self, event: Event, received_at: Instant) -> PublishOutcome {
        self.publish_with_observer(event, received_at, || {})
    }

    pub(super) fn publish_with_observer(
        &self,
        event: Event,
        received_at: Instant,
        observer: impl FnOnce(),
    ) -> PublishOutcome {
        self.publish_with_nonce(event, received_at, None, observer)
    }

    pub fn stop_accepting(&self) {
        self.accepting.store(false, Ordering::Release);
    }

    #[must_use]
    pub fn topology(&self) -> super::topology::GatewayTopology {
        self.topology.clone()
    }

    #[must_use]
    pub fn subscribe_identity(&self) -> GatewayIdentityReceiver {
        self.identity.subscribe_identity()
    }

    #[must_use]
    pub fn subscribe_identity_conflict(&self) -> GatewayIdentityConflictReceiver {
        self.identity.subscribe_conflict()
    }

    #[must_use]
    pub fn subscribe_diagnostics(&self) -> IngressDiagnosticsReceiver {
        IngressDiagnosticsReceiver::new(
            Arc::clone(&self.diagnostics),
            self.diagnostic_notifications.subscribe(),
        )
    }

    #[must_use]
    pub fn subscribe_message_gaps(&self) -> MessageGapReceiver {
        self.message_gaps.subscribe()
    }

    fn publish_interaction(
        &self,
        event: Box<InteractionCreate>,
        received_at: Instant,
    ) -> PublishOutcome {
        let tag = if self.accepting.load(Ordering::Acquire) {
            InteractionIngressTag::Normal
        } else {
            InteractionIngressTag::Stopping
        };
        let Some(sequence) = self.sequence.next() else {
            self.record_hard_drop();
            return PublishOutcome::InteractionHardDropped {
                reason: UnavailableReason::SequenceExhausted,
            };
        };
        let envelope = InteractionIngress {
            sequence,
            received_at,
            tag,
            event,
        };
        if tag == InteractionIngressTag::Stopping {
            return self.try_reserved(envelope);
        }
        match self.normal_interactions.try_send(envelope) {
            Ok(()) => PublishOutcome::InteractionAccepted { sequence, tag },
            Err(error) => {
                let mut envelope = error.into_inner();
                envelope.tag = InteractionIngressTag::Busy;
                self.try_reserved(envelope)
            }
        }
    }

    fn try_reserved(&self, envelope: InteractionIngress) -> PublishOutcome {
        let sequence = envelope.sequence;
        let tag = envelope.tag;
        match self.reserved_interactions.try_send(envelope) {
            Ok(()) => PublishOutcome::InteractionAccepted { sequence, tag },
            Err(error) => {
                let reason = types::unavailable_reason(&error);
                self.record_hard_drop();
                PublishOutcome::InteractionHardDropped { reason }
            }
        }
    }

    fn publish_message(&self, event: Box<MessageCreate>, nonce: Option<u64>) -> PublishOutcome {
        let publication = self.message_gaps.begin_publication();
        if !self.accepting.load(Ordering::Acquire) {
            return self.message_gap(&publication, &event, UnavailableReason::Stopping);
        }
        let Some(sequence) = self.sequence.next() else {
            return self.message_gap(&publication, &event, UnavailableReason::SequenceExhausted);
        };
        let messages = if is_emergency_message(&event.content) {
            &self.emergency_messages
        } else {
            &self.messages
        };
        match messages.try_send(MessageIngress {
            sequence,
            event,
            nonce,
        }) {
            Ok(()) => PublishOutcome::MessageAccepted { sequence },
            Err(error) => {
                let reason = types::unavailable_reason(&error);
                let envelope = error.into_inner();
                self.message_gap(&publication, &envelope.event, reason)
            }
        }
    }

    fn message_gap(
        &self,
        publication: &gaps::MessageGapPublication<'_>,
        event: &MessageCreate,
        reason: UnavailableReason,
    ) -> PublishOutcome {
        let tracking = publication.record(event, reason);
        self.record_recoverable_gap();
        PublishOutcome::MessageRecoverableGap { reason, tracking }
    }

    fn record_hard_drop(&self) {
        self.diagnostics.record_hard_drop();
        let _ = self.diagnostic_notifications.send(());
    }

    fn record_recoverable_gap(&self) {
        self.diagnostics.record_recoverable_gap();
        let _ = self.diagnostic_notifications.send(());
    }
}
