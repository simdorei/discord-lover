use cdr_app_server::AppServerError;
use cdr_codex_state::CodexStateError;
use cdr_discord::gateway::ingress::{
    InteractionIngressTag, MessageGapAckError, MessageGapStateError,
};
use cdr_discord::gateway::{
    GatewayActivationError, GatewayIdentityConflict, GatewayIngressReceiversError,
    GatewayShutdownError, GatewayStartError,
};
use cdr_discord::http::DiscordHttpError;
use cdr_remote_agent::restart_handoff::RestartHandoffError;
use cdr_remote_agent::runner::RemoteAgentRunError;
use cdr_store::StoreError;
use thiserror::Error;

use crate::bridge_state::BridgeStateError;
use crate::discord_dispatch::DiscordDispatchError;
use crate::history_poll::{HistoryPollCommitError, HistoryPollStateError};
use crate::message_worker::MessageAdmissionError;
use crate::queue_runner::QueueRunnerError;
use crate::restart_readiness::drain::DrainGateError;
use crate::restart_readiness::drain_controller::DrainProtocolError;
use crate::runtime_instance::RuntimeInstanceError;
use crate::runtime_paths::{PathDiscoveryError, RuntimePathError};

#[derive(Debug, Error)]
pub enum DiscordRuntimeError {
    #[error(transparent)]
    Agy(#[from] crate::agy::AgyError),
    #[error(transparent)]
    Discovery(#[from] PathDiscoveryError),
    #[error(transparent)]
    Paths(#[from] RuntimePathError),
    #[error(transparent)]
    RuntimeInstance(#[from] RuntimeInstanceError),
    #[error(transparent)]
    RestartDrainGate(#[from] DrainGateError),
    #[error(transparent)]
    RestartDrainProtocol(#[from] DrainProtocolError),
    #[error(transparent)]
    State(#[from] CodexStateError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    BridgeState(#[from] BridgeStateError),
    #[error(transparent)]
    AppServer(#[from] AppServerError),
    #[error(transparent)]
    Queue(#[from] QueueRunnerError),
    #[error(transparent)]
    GatewayStart(#[from] GatewayStartError),
    #[error(transparent)]
    GatewayShutdown(#[from] GatewayShutdownError),
    #[error(transparent)]
    GatewayActivation(#[from] GatewayActivationError),
    #[error(transparent)]
    GatewayIngressReceivers(#[from] GatewayIngressReceiversError),
    #[error(transparent)]
    GatewayIdentityConflict(#[from] GatewayIdentityConflict),
    #[error(transparent)]
    MessageGapState(#[from] MessageGapStateError),
    #[error(transparent)]
    MessageGapAck(#[from] MessageGapAckError),
    #[error(transparent)]
    HistoryPollState(#[from] HistoryPollStateError),
    #[error(transparent)]
    HistoryPollCommit(#[from] HistoryPollCommitError),
    #[error(transparent)]
    DiscordHttp(#[from] DiscordHttpError),
    #[error(transparent)]
    Dispatch(#[from] DiscordDispatchError),
    #[error(transparent)]
    MessageAdmission(#[from] MessageAdmissionError),
    #[error(transparent)]
    RestartHandoff(#[from] RestartHandoffError),
    #[error(transparent)]
    RemoteAgent(#[from] RemoteAgentRunError),
    #[error("Discord typed ingress {0} channel closed before shutdown")]
    TypedIngressClosed(&'static str),
    #[error("Discord typed ingress {0} shutdown drain exceeded its deadline")]
    TypedIngressDrainTimeout(&'static str),
    #[error("Discord typed ingress {0} worker exited before announcing readiness")]
    TypedIngressReadiness(&'static str),
    #[error("Discord typed ingress {lane} lane received unexpected {tag:?} item")]
    TypedInteractionTag {
        lane: &'static str,
        tag: InteractionIngressTag,
    },
    #[error("Discord message gap carried an invalid zero message identifier")]
    InvalidMessageGap,
    #[error("Discord history target carried an invalid zero channel identifier")]
    InvalidHistoryChannel,
    #[error("Discord history gap source returned {actual} messages; maximum is 10")]
    HistoryGapBatchTooLarge { actual: usize },
    #[error("system clock is before the Unix epoch: {0}")]
    SystemClock(#[from] std::time::SystemTimeError),
    #[error("system clock microseconds exceed the signed history timestamp range")]
    SystemClockRange,
    #[error("could not wait for Ctrl+C: {0}")]
    CtrlC(std::io::Error),
    #[error("could not determine the runtime working directory: {0}")]
    CurrentDirectory(std::io::Error),
    #[error("completed app-server fork handoffs contain a cycle from {thread_id}")]
    BridgeForkCycle { thread_id: String },
    #[error("durable prompt intake recovery failed: {0}")]
    PromptIntakeRecovery(String),
    #[error("mirror sync configuration failed: {0}")]
    MirrorSyncConfiguration(String),
    #[error("runtime worker {worker} exited before shutdown")]
    WorkerExited { worker: &'static str },
    #[error("runtime worker exit monitor closed before shutdown")]
    WorkerMonitorClosed,
    #[error("runtime worker {worker} task failed: {source}")]
    WorkerTask {
        worker: &'static str,
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("runtime worker shutdown exceeded its common deadline: {workers}")]
    WorkerShutdownTimeout { workers: String },
    #[error("Discord gateway shard {shard} exited before shutdown")]
    GatewayShardExited { shard: u32 },
    #[error("Discord gateway shard exit monitor closed before shutdown")]
    GatewayShardMonitorClosed,
}
