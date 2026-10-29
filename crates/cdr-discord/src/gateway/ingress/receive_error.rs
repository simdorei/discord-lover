use super::{GatewayIngress, ReceiveErrorIngress, ReceiveErrorPublishOutcome, unavailable_reason};

impl GatewayIngress {
    #[must_use]
    pub fn publish_receive_error(&self, shard: u32, message: String) -> ReceiveErrorPublishOutcome {
        self.topology.invalidate();
        let item = ReceiveErrorIngress { shard, message };
        match self.receive_errors.try_send(item) {
            Ok(()) => ReceiveErrorPublishOutcome::Accepted,
            Err(error) => {
                let reason = unavailable_reason(&error);
                let item = error.into_inner();
                eprintln!(
                    "Discord gateway receive error on shard {}: {}; typed receive-error lane dropped event: {reason:?}",
                    item.shard, item.message
                );
                ReceiveErrorPublishOutcome::Dropped { reason }
            }
        }
    }
}
