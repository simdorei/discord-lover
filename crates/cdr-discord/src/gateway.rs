use std::sync::Arc;

use thiserror::Error;
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};
use twilight_gateway::error::StartRecommendedError;
use twilight_http::Client;

mod activation;
mod config;
mod gateway_identity;
pub mod ingress;
mod runtime_publication;
mod shard;
mod shutdown;
pub mod topology;

pub use activation::GatewayActivationError;
pub use config::{gateway_event_flags, gateway_intents};
pub use gateway_identity::{
    GatewayIdentity, GatewayIdentityConflict, GatewayIdentityConflictReceiver,
    GatewayIdentityReceiver,
};
use ingress::{
    GatewayIngress, GatewayIngressConfig, GatewayIngressConfigError, GatewayIngressReceivers,
    IngressDiagnosticsReceiver, MessageGapReceiver,
};
pub use runtime_publication::GatewayIngressPublishOutcomeSnapshot;
use runtime_publication::GatewayIngressPublishOutcomes;
pub use shutdown::GatewayShutdownReport;
use shutdown::{
    GatewayTask, SHUTDOWN_TIMEOUT, join_gateway_tasks_for_cause, join_gateway_tasks_until,
};

#[derive(Debug, Error)]
pub enum GatewayStartError {
    #[error(transparent)]
    IngressConfig(#[from] GatewayIngressConfigError),
    #[error("failed to start recommended Discord gateway shards: {0}")]
    Recommended(#[from] StartRecommendedError),
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum GatewayIngressReceiversError {
    #[error("Discord gateway ingress receivers have already been taken")]
    AlreadyTaken,
}

#[derive(Debug, Error)]
pub enum GatewayShutdownError {
    #[error("Discord gateway shard task did not stop before the shutdown deadline")]
    Timeout,
    #[error("Discord gateway shard task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

pub struct GatewayRuntime {
    ingress: GatewayIngress,
    ingress_receivers: Option<GatewayIngressReceivers>,
    ingress_publish_outcomes: GatewayIngressPublishOutcomes,
    activation: activation::ShardActivation,
    shutdown: watch::Sender<bool>,
    tasks: Vec<GatewayTask>,
    shard_exits: mpsc::Receiver<u32>,
    http: Arc<Client>,
}

impl GatewayRuntime {
    #[must_use]
    pub fn topology(&self) -> topology::GatewayTopology {
        self.ingress.topology()
    }
    #[must_use]
    pub fn http(&self) -> Arc<Client> {
        Arc::clone(&self.http)
    }

    #[must_use]
    /// Read the current sticky snapshot before waiting for changes.
    pub fn subscribe_identity(&self) -> GatewayIdentityReceiver {
        self.ingress.subscribe_identity()
    }

    #[must_use]
    /// Read the current sticky conflict before waiting; any value is fatal.
    pub fn subscribe_identity_conflict(&self) -> GatewayIdentityConflictReceiver {
        self.ingress.subscribe_identity_conflict()
    }

    /// Transfer the typed ingress receivers to their single consumer set.
    ///
    /// With [`Self::start_paused`], spawn every typed consumer after this transfer and call
    /// [`Self::activate_typed_consumers`] only when they are ready.
    pub fn take_ingress_receivers(
        &mut self,
    ) -> Result<GatewayIngressReceivers, GatewayIngressReceiversError> {
        self.ingress_receivers
            .take()
            .ok_or(GatewayIngressReceiversError::AlreadyTaken)
    }

    #[must_use]
    pub fn subscribe_ingress_diagnostics(&self) -> IngressDiagnosticsReceiver {
        self.ingress.subscribe_diagnostics()
    }

    #[must_use]
    pub fn subscribe_message_gaps(&self) -> MessageGapReceiver {
        self.ingress.subscribe_message_gaps()
    }

    #[must_use]
    pub fn ingress_publish_outcomes(&self) -> GatewayIngressPublishOutcomeSnapshot {
        self.ingress_publish_outcomes.snapshot()
    }

    /// Marks the typed ingress boundary as stopping before consumer shutdown begins.
    ///
    /// Interactions decoded after this barrier use the reserved stopping lane, allowing callers
    /// to keep that lane alive long enough to send a protocol-valid unavailable response.
    pub fn begin_stopping(&self) {
        self.ingress.stop_accepting();
    }

    /// Resolves when any shard task exits, including unwinding after a panic.
    pub async fn wait_for_shard_exit(&mut self) -> Option<u32> {
        self.shard_exits.recv().await
    }

    pub async fn shutdown(self) -> Result<(), GatewayShutdownError> {
        self.shutdown_until(Instant::now() + SHUTDOWN_TIMEOUT).await
    }

    pub async fn shutdown_until(mut self, deadline: Instant) -> Result<(), GatewayShutdownError> {
        self.begin_stopping();
        self.activation.stop();
        let _ = self.shutdown.send(true);
        join_gateway_tasks_until(&mut self.tasks, deadline).await
    }

    pub async fn shutdown_for_cause(
        mut self,
        deadline: Instant,
        trigger: Option<u32>,
    ) -> GatewayShutdownReport {
        self.begin_stopping();
        self.activation.stop();
        let _ = self.shutdown.send(true);
        join_gateway_tasks_for_cause(&mut self.tasks, deadline, trigger).await
    }
}

impl Drop for GatewayRuntime {
    fn drop(&mut self) {
        self.ingress.stop_accepting();
        self.activation.stop();
        let _ = self.shutdown.send(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[cfg(test)]
impl GatewayRuntime {
    fn new_offline_for_test(config: GatewayIngressConfig) -> Result<Self, GatewayStartError> {
        let (ingress, ingress_receivers) = GatewayIngress::new(config)?;
        let (shutdown, _shutdown_receiver) = watch::channel(false);
        let (_shard_exit_sender, shard_exits) = mpsc::channel(1);
        Ok(Self {
            ingress,
            ingress_receivers: Some(ingress_receivers),
            ingress_publish_outcomes: GatewayIngressPublishOutcomes::default(),
            activation: activation::ShardActivation::paused(),
            shutdown,
            tasks: Vec::new(),
            shard_exits,
            http: Arc::new(Client::new("offline-token".into())),
        })
    }
}

#[cfg(test)]
#[path = "gateway_activation_api_tests.rs"]
mod activation_api_tests;
#[cfg(test)]
#[path = "gateway_activation_tests.rs"]
mod activation_tests;
#[cfg(test)]
#[path = "gateway_activation_waiter_tests.rs"]
mod activation_waiter_tests;
#[cfg(test)]
#[path = "gateway_identity_conflict_tests.rs"]
mod identity_conflict_tests;
#[cfg(test)]
#[path = "gateway_identity_tests.rs"]
mod identity_tests;
#[cfg(test)]
#[path = "gateway_ingress_edge_tests.rs"]
mod ingress_edge_tests;
#[cfg(test)]
#[path = "gateway_ingress_tests.rs"]
mod ingress_tests;
#[cfg(test)]
#[path = "gateway_message_gap_edge_tests.rs"]
mod message_gap_edge_tests;
#[cfg(test)]
#[path = "gateway_message_gap_mutation_tests.rs"]
mod message_gap_mutation_tests;
#[cfg(test)]
#[path = "gateway_message_gap_tests.rs"]
mod message_gap_tests;
#[cfg(test)]
#[path = "gateway_receive_error_tests.rs"]
mod receive_error_tests;
#[cfg(test)]
#[path = "gateway_runtime_ingress_tests.rs"]
mod runtime_ingress_tests;
#[cfg(test)]
#[path = "gateway_runtime_publication_concurrency_tests.rs"]
mod runtime_publication_concurrency_tests;
#[cfg(test)]
#[path = "gateway_runtime_publication_edge_tests.rs"]
mod runtime_publication_edge_tests;
#[cfg(test)]
#[path = "gateway_runtime_publication_failure_tests.rs"]
mod runtime_publication_failure_tests;
#[cfg(test)]
#[path = "gateway_runtime_publication_tests.rs"]
mod runtime_publication_tests;
#[cfg(test)]
#[path = "gateway_runtime_shutdown_tests.rs"]
mod runtime_shutdown_tests;
