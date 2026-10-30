use cdr_app_server::AppServerError;
use cdr_codex_state::{CodexStateError, ThreadResolveError};
use cdr_store::StoreError;
use thiserror::Error;

use crate::archive_delete::ArchiveDeleteError;
use crate::bridge_state::BridgeStateError;
use crate::prompt_preprocessor::PromptPreprocessError;
use crate::queue_runner::QueueRunnerError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionResult {
    pub text: String,
    pub waits_for_final: bool,
    pub ui: Option<ActionUi>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionUi {
    PendingBusy {
        choice_id: String,
    },
    ProBusy {
        choice_id: String,
    },
    ServerPrompts {
        prompts: Vec<crate::server_prompt_redisplay::PreparedPrompt>,
    },
    Busy {
        choice_id: String,
        allow_steer: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionContext {
    pub channel_id: u64,
    pub user_id: u64,
    pub discord_message_id: Option<u64>,
    pub auto_queue_when_busy: bool,
}

#[derive(Debug, Error)]
pub enum ActionError {
    #[error(transparent)]
    PromptRedisplay(#[from] crate::server_prompt_redisplay::RedisplayError),
    #[error(transparent)]
    State(#[from] CodexStateError),
    #[error(transparent)]
    Resolve(#[from] ThreadResolveError),
    #[error(transparent)]
    Bridge(#[from] BridgeStateError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Queue(#[from] QueueRunnerError),
    #[error(transparent)]
    AppServer(#[from] AppServerError),
    #[error(transparent)]
    ArchiveDelete(#[from] ArchiveDeleteError),
    #[error(transparent)]
    Prompt(#[from] PromptPreprocessError),
    #[error(transparent)]
    MirrorSync(#[from] crate::mirror_sync::MirrorSyncError),
    #[error("no Codex thread target is selected or mirrored for this channel")]
    NoTarget,
    #[error("Discord identifier does not fit the SQLite integer contract")]
    IntegerRange,
    #[error("system clock is before the Unix epoch: {0}")]
    SystemTime(#[from] std::time::SystemTimeError),
    #[error("operating-system command failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("resident Codex app-server is unavailable for this command")]
    MissingAppServer,
    #[error("invalid command request: {0}")]
    Invalid(String),
    #[error("runtime action is not implemented yet: {0}")]
    Unsupported(&'static str),
}
