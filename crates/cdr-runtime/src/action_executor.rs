use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::archive_delete::ArchiveDeletePaths;
use crate::bridge_state::BridgeState;
use crate::command_plan::CommandAction;
use crate::prompt_preprocessor::PromptPreprocessor;
use crate::queue_runner::{QueueCoordinator, TurnBackend};
use cdr_app_server::ResidentAppServer;

mod app_server_requests;
mod app_server_target;
mod archive_action;
mod archive_context;
mod archive_delete_action;
mod archive_scope;
mod context_action;
mod control_actions;
mod control_turn;
mod format;
mod ingress_inspection;
mod lifecycle_custody;
mod list_view;
mod mirror_action;
pub(crate) mod model_catalog;
mod new_thread;
mod operator_actions;
mod prompt_intake;
mod queue_actions;
mod queue_result;
mod queue_submission;
mod recovery_custody;
mod repair_action;
mod resume_action;
mod retract_action;
mod runner_target;
mod runners_status;
mod selection;
mod service_actions;
pub(crate) mod settings_action;
mod settings_custody;
mod stop_action;
mod stop_recovery;
mod stop_worker;
mod target_resolution;
mod types;
mod usage_format;
mod writer_recovery_action;

use format::{HELP, immediate};
pub use types::{ActionContext, ActionError, ActionResult, ActionUi};

pub struct ActionExecutor<B: TurnBackend> {
    state_db: PathBuf,
    mirror_db: PathBuf,
    bridge_state: Arc<BridgeState>,
    queue: Arc<QueueCoordinator<B>>,
    server: Option<Arc<ResidentAppServer>>,
    app_server_resume_timeout: Duration,
    host_commands: bool,
    archive_delete_paths: ArchiveDeletePaths,
    prompt_preprocessor: Option<Arc<dyn PromptPreprocessor>>,
    mirror_sync: std::sync::OnceLock<crate::mirror_sync::MirrorSynchronizer>,
    #[cfg(test)]
    recovery_root: Option<PathBuf>,
}

impl<B: TurnBackend> ActionExecutor<B> {
    #[must_use]
    pub fn new(
        state_db: PathBuf,
        mirror_db: PathBuf,
        bridge_state: Arc<BridgeState>,
        queue: Arc<QueueCoordinator<B>>,
    ) -> Self {
        let codex_home = state_db
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_owned();
        let archive_delete_paths = ArchiveDeletePaths {
            state_db: state_db.clone(),
            log_db: codex_home.join("logs_2.sqlite"),
            global_state: codex_home.join(".codex-global-state.json"),
            bridge_state: bridge_state.path().to_owned(),
            session_index: codex_home.join("session_index.jsonl"),
            archived_sessions: codex_home.join("archived_sessions"),
            backup_root: codex_home.join("maintenance_backups"),
        };
        Self {
            state_db,
            mirror_db,
            bridge_state,
            queue,
            server: None,
            app_server_resume_timeout: Duration::from_mins(1),
            host_commands: false,
            archive_delete_paths,
            prompt_preprocessor: None,
            mirror_sync: std::sync::OnceLock::new(),
            #[cfg(test)]
            recovery_root: None,
        }
    }

    #[must_use]
    pub fn with_server(mut self, server: Arc<ResidentAppServer>) -> Self {
        self.server = Some(server);
        self
    }

    #[must_use]
    pub const fn with_host_commands(mut self, enabled: bool) -> Self {
        self.host_commands = enabled;
        self
    }

    #[must_use]
    pub fn with_archive_delete_paths(mut self, paths: ArchiveDeletePaths) -> Self {
        self.archive_delete_paths = paths;
        self
    }

    #[must_use]
    pub fn with_prompt_preprocessor(mut self, value: Arc<dyn PromptPreprocessor>) -> Self {
        self.prompt_preprocessor = Some(value);
        self
    }

    async fn prepare_prompt(&self, prompt: &str, thread_id: &str) -> Result<String, ActionError> {
        match &self.prompt_preprocessor {
            Some(preprocessor) => Ok(preprocessor.prepare(prompt, thread_id).await?),
            None => Ok(prompt.to_owned()),
        }
    }

    #[must_use]
    pub fn mirror_db(&self) -> &Path {
        &self.mirror_db
    }

    pub fn settings_resolver(&self) -> crate::settings_binding::SettingsTargetResolver {
        crate::settings_binding::SettingsTargetResolver::new(
            self.state_db.clone(),
            self.mirror_db.clone(),
            self.bridge_state.clone(),
        )
    }

    pub(crate) fn notify_delivery_ready(&self) {
        self.queue.notify_delivery_ready();
    }

    pub fn target_thread_id(&self, channel_id: u64) -> Result<String, ActionError> {
        self.target(channel_id).map(|(thread_id, _)| thread_id)
    }

    pub async fn execute(
        &self,
        action: CommandAction,
        channel_id: u64,
        user_id: u64,
    ) -> Result<ActionResult, ActionError> {
        self.execute_with_context(
            action,
            ActionContext {
                channel_id,
                user_id,
                discord_message_id: None,
                auto_queue_when_busy: false,
            },
        )
        .await
    }

    pub async fn execute_with_context(
        &self,
        action: CommandAction,
        context: ActionContext,
    ) -> Result<ActionResult, ActionError> {
        if matches!(
            action,
            CommandAction::New { .. } | CommandAction::Ask { .. } | CommandAction::Interview { .. }
        ) {
            return self.execute_prompt_action(action, context).await;
        }
        let (channel_id, user_id) = (context.channel_id, context.user_id);
        let result = match action {
            CommandAction::Help => immediate(HELP),
            CommandAction::List { limit } => return self.thread_list(limit, false).await,
            CommandAction::ArchivedList { limit } => return self.thread_list(limit, true).await,
            CommandAction::Use { reference } => immediate(self.select(&reference)?),
            CommandAction::Where => immediate(self.where_message(channel_id)?),
            CommandAction::Status { reference } => {
                return self.status(channel_id, reference.as_deref()).await;
            }
            action @ (CommandAction::Settings { .. } | CommandAction::AutoReserve { .. }) => {
                return self.settings_command(channel_id, action).await;
            }
            CommandAction::Context {
                all_threads,
                refresh,
                limit,
            } => return self.context(channel_id, all_threads, refresh, limit).await,
            CommandAction::Usage { days } => return self.usage(days).await,
            CommandAction::New { .. }
            | CommandAction::Ask { .. }
            | CommandAction::Interview { .. } => {
                unreachable!("prompt actions returned before the main dispatch")
            }
            CommandAction::Retract { reference } => {
                immediate(self.retract_prompt(channel_id, user_id, reference.as_deref())?)
            }
            CommandAction::Runners => immediate(self.runners_for_actor(channel_id, user_id).await?),
            CommandAction::SavedRequest { request_id } => {
                immediate(self.saved_request_message(channel_id, user_id, &request_id)?)
            }
            CommandAction::DiscardRequest { .. } => {
                return Err(ActionError::Invalid(
                    "discard-request requires authenticated message custody and live normal admission".into(),
                ));
            }
            CommandAction::Doctor => return self.doctor().await,
            CommandAction::MirrorCheck => {
                return self.inspect_mirror(channel_id, None, false).await;
            }
            CommandAction::MirrorInspect { limit, list } => {
                return self.inspect_mirror(channel_id, limit, list).await;
            }
            CommandAction::BridgeSync { limit } => {
                return self.bridge_sync(context, limit).await;
            }
            CommandAction::QaButtons => immediate(
                "Rust button QA contracts are enabled: busy, approval, input, one-use claim, and retry release.",
            ),
            CommandAction::Open { reference, abort } => {
                return self.open_thread(&reference, abort).await;
            }
            CommandAction::Stop { reference } => {
                return self.stop_thread(context, reference.as_deref()).await;
            }
            CommandAction::Recover { reference } => {
                return self.recover_writer(context, reference.as_deref()).await;
            }
            CommandAction::Repair { reference } => {
                return self
                    .repair_tools(context.channel_id, reference.as_deref())
                    .await;
            }
            CommandAction::SettingsOptions { reference, field } => {
                return self
                    .settings_options(channel_id, reference.as_deref(), field.as_deref())
                    .await;
            }
            CommandAction::RestartCodex => return self.restart_codex().await,
            CommandAction::ForceRestartCodex => return Self::force_restart_codex(),
            CommandAction::Archive { reference } => {
                return self.archive_thread(context, reference.as_deref()).await;
            }
            CommandAction::DeleteArchivePreview { reference } => {
                immediate(self.delete_archive_preview(&reference)?)
            }
            CommandAction::DeleteArchiveConfirm { reference } => {
                return self.delete_archive_confirm(&reference, context);
            }
            CommandAction::Resume { reference } => {
                return self.resume_thread(channel_id, reference.as_deref()).await;
            }
            CommandAction::Identity => immediate(format!(
                "Discord identity\nuser_id: {user_id}\nchannel_id: {channel_id}\nthread_id: {}",
                self.target(channel_id)?.0
            )),
            CommandAction::Resources => return self.resources().await,
            CommandAction::Approval => return self.approval(channel_id, user_id).await,
            CommandAction::Steer { prompt } => return self.steer(channel_id, &prompt).await,
            CommandAction::HostReboot => return self.host_reboot().await,
        };
        Ok(result)
    }
}

fn id_i64(id: u64) -> Result<i64, ActionError> {
    i64::try_from(id).map_err(|_| ActionError::IntegerRange)
}
