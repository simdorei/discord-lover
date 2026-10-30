use std::sync::Arc;
use std::time::Duration;

use cdr_app_server::requests::{
    fork_thread_persistent, resume_thread_with_timeout, start_turn, start_turn_with_input,
};
use cdr_app_server::{AppServerError, ResidentAppServer, extract_thread_id};
use serde_json::Value;

mod async_history;
mod async_terminal;
mod fresh_thread;
pub(crate) mod turn_index;

use crate::queue_runner::{BackendFailure, BoxBackendFuture, TurnBackend, TurnRecord};

pub struct AppServerTurnBackend {
    server: Arc<ResidentAppServer>,
    pro_skill_path: Option<std::path::PathBuf>,
    resume_timeout: Duration,
    history_read_timeout: Duration,
    fresh_threads: fresh_thread::FreshThreads,
}

impl AppServerTurnBackend {
    #[must_use]
    pub const fn new(server: Arc<ResidentAppServer>) -> Self {
        Self {
            server,
            pro_skill_path: None,
            resume_timeout: Duration::from_mins(1),
            history_read_timeout: Duration::from_mins(1),
            fresh_threads: fresh_thread::FreshThreads::new(),
        }
    }

    #[must_use]
    pub fn with_pro_skill_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.pro_skill_path = Some(path.into());
        self
    }

    #[must_use]
    pub const fn with_timeouts(mut self, resume: Duration, history_read: Duration) -> Self {
        self.resume_timeout = resume;
        self.history_read_timeout = history_read;
        self
    }

    async fn start_with_claim(
        &self,
        thread_id: &str,
        prompt: &str,
        claimed: Option<&cdr_store::queue::StoredQueueJob>,
    ) -> Result<String, BackendFailure> {
        let request = self.pro_skill_path.as_ref().map_or_else(
            || start_turn(thread_id, prompt),
            |path| {
                start_turn_with_input(thread_id, &cdr_pro::prompt::build_turn_input(prompt, path))
            },
        );
        self.fresh_threads.consume(thread_id);
        let result = if let Some(claimed) = claimed {
            let generation = u64::try_from(claimed.app_server_generation)
                .map_err(|error| BackendFailure::definite(error.to_string()))?;
            let authority = serde_json::to_value(claimed)
                .map_err(|error| BackendFailure::definite(error.to_string()))?;
            self.server
                .execute_queue_turn(request, generation, authority)
                .await
        } else {
            self.server.execute(request, Some(self.generation())).await
        }
        .map_err(|error| {
            if claimed.is_some() && matches!(error, AppServerError::MutationHeld { .. }) {
                // A failed authority/DB check cannot grant a retry or rewind
                // Starting. Recovery may already own or have removed the row.
                BackendFailure::ambiguous(error.to_string())
            } else {
                start_failure(&error)
            }
        })?;
        result
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|turn_id| !turn_id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| BackendFailure::ambiguous("turn/start returned no turn id"))
    }
}

impl TurnBackend for AppServerTurnBackend {
    fn resident_instance_id(&self) -> Option<&str> {
        Some(self.server.instance_id())
    }
    fn remember_new_thread(&self, thread_id: &str, generation: u64) {
        self.fresh_threads.remember(thread_id, generation);
    }
    fn generation(&self) -> u64 {
        self.server.generation()
    }

    fn requires_app_server_fork(&self) -> bool {
        false
    }

    fn active_turn_id<'a>(&'a self, thread_id: &'a str) -> BoxBackendFuture<'a, Option<String>> {
        Box::pin(async move {
            self.server
                .active_turn_id(thread_id)
                .await
                .map_err(|error| definite(&error))
        })
    }

    fn read_turns<'a>(&'a self, thread_id: &'a str) -> BoxBackendFuture<'a, Vec<TurnRecord>> {
        Box::pin(async move {
            if self.fresh_threads.contains(thread_id, self.generation()) {
                return Ok(Vec::new());
            }
            self.read_turn_index(thread_id).await
        })
    }

    fn resume_thread<'a>(&'a self, thread_id: &'a str) -> BoxBackendFuture<'a, ()> {
        Box::pin(async move {
            if self.fresh_threads.contains(thread_id, self.generation()) {
                return Ok(());
            }
            // This path only consumes identity; history is read separately below.
            // Avoid hydrating a large rollout just to open its subscription.
            let mut request = resume_thread_with_timeout(thread_id, self.resume_timeout);
            request.params["excludeTurns"] = Value::Bool(true);
            let result = self
                .server
                .execute(request, Some(self.generation()))
                .await
                .map_err(|error| resume_failure(&error))?;
            validate_thread_identity(&result, thread_id).map_err(BackendFailure::definite)?;
            Ok(())
        })
    }

    fn read_async_history<'a>(
        &'a self,
        thread_id: &'a str,
        originals: &'a [String],
    ) -> BoxBackendFuture<'a, Option<Value>> {
        Box::pin(async move {
            self.read_async_resolution_history(thread_id, originals)
                .await
        })
    }

    fn read_async_terminal<'a>(
        &'a self,
        thread_id: &'a str,
        owners: &'a [String],
    ) -> BoxBackendFuture<'a, Option<Value>> {
        Box::pin(async move { self.read_async_terminal_history(thread_id, owners).await })
    }

    fn read_recovery_prerequisites<'a>(
        &'a self,
        thread_id: &'a str,
        owners: &'a [String],
    ) -> BoxBackendFuture<'a, cdr_app_server::NativeRecoveryObservation> {
        Box::pin(async move {
            self.server
                .observe_recovery_prerequisites(thread_id, owners, self.history_read_timeout)
                .await
                .map_err(|error| definite(&error))
        })
    }

    fn fork_thread<'a>(&'a self, thread_id: &'a str) -> BoxBackendFuture<'a, String> {
        Box::pin(async move {
            let result = self
                .server
                .execute(
                    fork_thread_persistent(thread_id, self.resume_timeout),
                    Some(self.generation()),
                )
                .await
                .map_err(|error| mutation_failure(&error))?;
            let forked = extract_thread_id(&result)
                .filter(|candidate| candidate != thread_id)
                .ok_or_else(|| {
                    BackendFailure::ambiguous("thread/fork returned no distinct thread id")
                })?;
            Ok(forked)
        })
    }

    fn start_turn<'a>(
        &'a self,
        thread_id: &'a str,
        prompt: &'a str,
    ) -> BoxBackendFuture<'a, String> {
        Box::pin(self.start_with_claim(thread_id, prompt, None))
    }

    fn start_claimed_turn<'a>(
        &'a self,
        claimed: &'a cdr_store::queue::StoredQueueJob,
    ) -> BoxBackendFuture<'a, String> {
        Box::pin(self.start_with_claim(&claimed.target_thread_id, &claimed.prompt, Some(claimed)))
    }
}

fn definite(error: &AppServerError) -> BackendFailure {
    BackendFailure::definite(error.to_string())
}

fn resume_failure(error: &AppServerError) -> BackendFailure {
    if matches!(
        error,
        AppServerError::MutationHeld { .. } | AppServerError::MutationOutcomeUnknown { .. }
    ) {
        return BackendFailure::execution_held(error.to_string());
    }
    if matches!(error, AppServerError::IdleRelease { .. })
        && error
            .to_string()
            .contains(cdr_store::async_resolution::HOLD_PREFIX)
    {
        return BackendFailure::execution_held(error.to_string());
    }

    if let AppServerError::Remote {
        method,
        code: -32_600,
        message,
        ..
    } = error
        && method == "thread/resume"
        && message.contains("already has an active writer")
    {
        return BackendFailure::active_writer(error.to_string());
    }
    definite(error)
}

fn start_failure(error: &AppServerError) -> BackendFailure {
    if matches!(error, AppServerError::MutationHeld { .. }) {
        return BackendFailure::execution_held(error.to_string());
    }
    if matches!(error, AppServerError::IdleRelease { .. })
        && error
            .to_string()
            .contains(cdr_store::async_resolution::HOLD_PREFIX)
    {
        return BackendFailure::execution_held(error.to_string());
    }

    if let AppServerError::Remote { data, .. } = error
        && cdr_app_server::is_usage_limit_error(data.as_ref())
    {
        return BackendFailure::usage_limit(error.to_string());
    }
    mutation_failure(error)
}

fn mutation_failure(error: &AppServerError) -> BackendFailure {
    if matches!(error, AppServerError::MutationHeld { .. }) {
        return BackendFailure::execution_held(error.to_string());
    }
    if matches!(error, AppServerError::IdleRelease { .. })
        && error
            .to_string()
            .contains(cdr_store::async_resolution::HOLD_PREFIX)
    {
        return BackendFailure::execution_held(error.to_string());
    }

    let ambiguous = matches!(
        error,
        AppServerError::Io(_)
            | AppServerError::MutationOutcomeUnknown { .. }
            | AppServerError::Json(_)
            | AppServerError::Timeout { .. }
            | AppServerError::TransportClosed { .. }
            | AppServerError::ResponseChannelClosed { .. }
            | AppServerError::Closed
    );
    if ambiguous {
        BackendFailure::ambiguous(error.to_string())
    } else {
        BackendFailure::definite(error.to_string())
    }
}

fn validate_thread_identity(result: &Value, expected: &str) -> Result<(), String> {
    let actual = result
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str);
    if actual == Some(expected) {
        Ok(())
    } else {
        Err("thread/resume returned a different or invalid thread".into())
    }
}

#[cfg(test)]
mod tests;
