use tokio::time::Instant;
use twilight_model::gateway::event::Event;

use super::{GatewayIngress, PublishOutcome};
use crate::gateway::gateway_identity::publish_gateway_event_identity;

impl GatewayIngress {
    pub(in crate::gateway) fn publish_with_nonce(
        &self,
        event: Event,
        received_at: Instant,
        nonce: Option<u64>,
        observer: impl FnOnce(),
    ) -> PublishOutcome {
        self.topology.publish(&event);
        publish_gateway_event_identity(&event, &self.identity);
        observer();
        match event {
            Event::InteractionCreate(event) => self.publish_interaction(event, received_at),
            Event::MessageCreate(event) => self.publish_message(event, nonce),
            _ => PublishOutcome::Ignored,
        }
    }
}
