use thiserror::Error;

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("이전 답변 전달 또는 대기 요청이 남아 있습니다. 처리가 끝난 뒤 다시 답해 주세요.")]
    AsyncQuestionPendingWork,
    #[error("[cdr-rust:async-resolution-held:v1] {thread_id}: {reason}")]
    AsyncResolutionHeld { thread_id: String, reason: String },
    #[error("room {channel} protected by {reason}")]
    CleanupProtected { channel: i64, reason: &'static str },
    #[error(
        "request {0} was cancelled by its original sender; a late result cannot replace the cancellation"
    )]
    RequestCancelled(String),
    #[error("conversation {0} is on hold after app-server process loss; manual review is required")]
    DeadGenerationTargetHeld(String),
    #[error("SQLite store error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("JSON store value error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("system clock is before the Unix epoch: {0}")]
    SystemTime(#[from] std::time::SystemTimeError),
    #[error("store schema version {found} is newer than supported version {supported}")]
    UnsupportedVersion { found: i64, supported: i64 },
    #[error(
        "read-only store inspection requires schema version {expected}; found {found}; no migration performed"
    )]
    ReadOnlySchemaVersion { found: i64, expected: i64 },
    #[error("SQLite integrity check failed: {0}")]
    Integrity(String),
    #[error("store schema migration requires a connection without an active transaction")]
    ActiveTransaction,
    #[error("durable queue job not found: {0}")]
    QueueJobNotFound(String),
    #[error("busy choice is expired, changed, or already claimed: {0}")]
    BusyChoiceUnavailable(String),
    #[error("invalid durable queue state: {0}")]
    InvalidQueueState(String),
    #[error("durable queue job has no turn id: {0}")]
    QueueJobHasNoTurn(String),
    #[error("durable Discord delivery not found: {0}")]
    DeliveryNotFound(String),
    #[error("mirror thread is not mapped: {0}")]
    MirrorThreadNotFound(String),
    #[error(
        "mirror mapping changed for Discord channel {discord_channel_id}: expected {expected_target_thread_id}, actual {actual_target_thread_id:?}"
    )]
    MirrorMappingChanged {
        discord_channel_id: i64,
        expected_target_thread_id: String,
        actual_target_thread_id: Option<String>,
    },
    #[error(
        "app-server fork handoff is unresolved for target thread {target_thread_id}: {last_error:?}"
    )]
    ForkHandoffUnresolved {
        target_thread_id: String,
        last_error: Option<String>,
    },
    #[error("app-server fork handoff moved source thread {source_thread_id} to {target_thread_id}")]
    ForkHandoffTargetMoved {
        source_thread_id: String,
        target_thread_id: String,
    },
    #[error("invalid direct app-server managed target: {0}")]
    InvalidAppServerManagedTarget(String),
    #[error("durable prompt intake not found: {0}")]
    PromptIntakeNotFound(String),
    #[error("invalid durable prompt intake identity: job={job_id:?}, target={target_thread_id:?}")]
    InvalidPromptIntakeIdentity {
        job_id: String,
        target_thread_id: String,
    },
    #[error(
        "durable prompt intake identity conflict: job={job_id}, Discord message={discord_message_id:?}"
    )]
    PromptIntakeIdentityConflict {
        job_id: String,
        discord_message_id: Option<i64>,
    },
    #[error("durable prompt intake claim is no longer current: job={job_id}")]
    PromptIntakeClaimLost { job_id: String },
    #[error(
        "invalid durable prompt intake claim lease: now={now}, claim_expires_at={claim_expires_at}"
    )]
    InvalidPromptIntakeLease { now: f64, claim_expires_at: f64 },
    #[error("invalid durable prompt intake retry timestamp: {0}")]
    InvalidPromptIntakeRetry(f64),
    #[error("completed app-server fork handoff cycle from {0}")]
    ForkHandoffCycle(String),
    #[error("invalid session mirror detail mode: {0}")]
    InvalidMirrorDetail(String),
}
