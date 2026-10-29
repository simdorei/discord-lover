use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use tokio::sync::broadcast;
use twilight_model::id::{Id, marker::ChannelMarker};

mod fence;
mod publication;
mod types;

pub use fence::*;
pub(super) use publication::MessageGapPublication;
pub use types::*;

const NOTIFICATION_CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug)]
struct ActiveGap {
    earliest: MessageGapPosition,
    reasons: MessageGapReasons,
    observation_count: u64,
}

#[derive(Debug, Default)]
struct Entry {
    revision: u64,
    active: Option<ActiveGap>,
}

#[derive(Clone, Debug)]
pub(super) struct MessageGapTracker {
    entries: Arc<Mutex<BTreeMap<Id<ChannelMarker>, Entry>>>,
    identity: Arc<()>,
    notifications: broadcast::Sender<()>,
    publication_gate: Arc<Mutex<()>>,
}

impl MessageGapTracker {
    pub(super) fn new() -> Self {
        let (notifications, _) = broadcast::channel(NOTIFICATION_CAPACITY);
        Self {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            identity: Arc::new(()),
            notifications,
            publication_gate: Arc::new(Mutex::new(())),
        }
    }

    fn snapshot(&self) -> Result<Vec<MessageGapNotice>, MessageGapStateError> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| MessageGapStateError::Poisoned)?;
        Ok(entries
            .iter()
            .filter_map(|(&channel_id, entry)| {
                entry.active.map(|active| MessageGapNotice {
                    snapshot: MessageGapSnapshot {
                        channel_id,
                        earliest: active.earliest,
                        reasons: active.reasons,
                        observation_count: active.observation_count,
                        revision: entry.revision,
                    },
                    tracker_identity: Arc::clone(&self.identity),
                })
            })
            .collect())
    }

    fn acknowledge(
        &self,
        notice: MessageGapNotice,
    ) -> Result<MessageGapAckOutcome, MessageGapAckError> {
        let MessageGapNotice {
            snapshot,
            tracker_identity,
        } = notice;
        if !Arc::ptr_eq(&self.identity, &tracker_identity) {
            return Err(MessageGapAckError::ForeignTracker);
        }
        if snapshot.revision == u64::MAX {
            return Err(MessageGapAckError::RevisionExhausted);
        }
        let cleared = {
            let mut entries = self
                .entries
                .lock()
                .map_err(|_| MessageGapAckError::StatePoisoned)?;
            let Some(entry) = entries.get_mut(&snapshot.channel_id) else {
                return Ok(MessageGapAckOutcome::Stale);
            };
            if entry.revision != snapshot.revision || entry.active.is_none() {
                false
            } else {
                entry.active = None;
                true
            }
        };
        if cleared {
            let _ = self.notifications.send(());
            Ok(MessageGapAckOutcome::Cleared)
        } else {
            Ok(MessageGapAckOutcome::Stale)
        }
    }

    pub(super) fn subscribe(&self) -> MessageGapReceiver {
        MessageGapReceiver {
            tracker: self.clone(),
            notifications: self.notifications.subscribe(),
        }
    }

    #[cfg(test)]
    pub(super) fn force_limits(
        &self,
        channel_id: Id<ChannelMarker>,
        observation_count: u64,
        revision: u64,
    ) -> Result<(), MessageGapStateError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| MessageGapStateError::Poisoned)?;
        let entry = entries.get_mut(&channel_id).expect("test gap exists");
        entry.revision = revision;
        entry
            .active
            .as_mut()
            .expect("test gap is active")
            .observation_count = observation_count;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn poison(&self) {
        let entries = Arc::clone(&self.entries);
        let _ = std::thread::spawn(move || {
            let _guard = entries.lock().expect("test lock starts healthy");
            panic!("poison message gap state for test");
        })
        .join();
    }
}

#[derive(Debug)]
pub struct MessageGapReceiver {
    tracker: MessageGapTracker,
    notifications: broadcast::Receiver<()>,
}

impl MessageGapReceiver {
    #[must_use]
    pub fn resubscribe(&self) -> Self {
        self.tracker.subscribe()
    }
    /// Read authoritative state before waiting; bounded notifications are only change hints.
    pub fn snapshot(&self) -> Result<Vec<MessageGapNotice>, MessageGapStateError> {
        self.tracker.snapshot()
    }

    /// Wait for a hint, then call [`Self::snapshot`] even after notification lag.
    pub async fn changed(&mut self) -> Result<(), broadcast::error::RecvError> {
        self.notifications.recv().await
    }

    /// Poll for a hint, then call [`Self::snapshot`] even after notification lag.
    pub fn try_changed(&mut self) -> Result<(), broadcast::error::TryRecvError> {
        self.notifications.try_recv()
    }

    /// Consumes the notice so one token cannot be acknowledged twice.
    ///
    /// ```compile_fail
    /// use cdr_discord::gateway::ingress::{MessageGapNotice, MessageGapReceiver};
    /// fn acknowledge_twice(receiver: &MessageGapReceiver, notice: MessageGapNotice) {
    ///     let _ = receiver.acknowledge(notice);
    ///     let _ = receiver.acknowledge(notice);
    /// }
    /// ```
    pub fn acknowledge(
        &self,
        notice: MessageGapNotice,
    ) -> Result<MessageGapAckOutcome, MessageGapAckError> {
        self.tracker.acknowledge(notice)
    }
}
