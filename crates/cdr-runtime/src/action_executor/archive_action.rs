use super::operator_actions::original_owner_error;
use super::{ActionError, ActionExecutor, ActionResult, immediate};
use crate::queue_runner::TurnBackend;
use cdr_app_server::requests::archive_thread;
use std::collections::BTreeSet;
use std::time::Duration;
#[path = "archive_dispatch.rs"]
mod dispatch;

#[cfg(test)]
#[path = "archive_refusal_tests.rs"]
mod refusal_tests;

impl<B: TurnBackend> ActionExecutor<B> {
    pub(super) async fn archive_thread(
        &self,
        context: super::ActionContext,
        reference: Option<&str>,
    ) -> Result<ActionResult, ActionError> {
        self.archive_bound(context, reference, None).await
    }

    pub(super) async fn archive_bound(
        &self,
        context: super::ActionContext,
        reference: Option<&str>,
        binding: Option<&crate::settings_binding::SettingsBinding>,
    ) -> Result<ActionResult, ActionError> {
        let own = self.archive_own_request(context, reference)?;
        let mut archive_sent = false;
        let result = tokio::time::timeout(
            self.app_server_resume_timeout,
            self.archive_with_verification(
                context,
                reference,
                &mut archive_sent,
                binding,
                own.as_deref(),
            ),
        )
        .await
        .map_err(|_| {
            let phase = if archive_sent {
                "archive dispatch was attempted; delivery and outcome is unverified and some conversations may already be archived; do not automatically retry"
            } else {
                "no archive was sent"
            };
            ActionError::Invalid(format!("archive operation timed out; {phase}"))
        })
        .and_then(|result| result);
        if !archive_sent
            && let (Some(key), Err(error)) = (own.as_deref(), &result)
            && matches!(error, ActionError::Invalid(_))
        {
            // A known refusal is a finished command, even if its error notice
            // is lost. Preserve that fact before MessageCustody can retain it
            // as an unknown lifecycle action and fence every later request.
            // An attempted archive keeps its existing fence and unknown result.
            // Typed app-server holds also retain custody: a stop revision or
            // uncertain mutation is not a locally completed refusal.
            cdr_store::ingress::record_result(
                &self.mirror_db,
                key,
                &serde_json::json!({
                    "kind": "archive_not_dispatched",
                    "version": 1,
                    "archive_dispatched": false,
                    "error": error.to_string(),
                }),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs_f64(),
            )?;
        }
        result
    }

    async fn archive_with_verification(
        &self,
        context: super::ActionContext,
        reference: Option<&str>,
        archive_sent: &mut bool,
        binding: Option<&crate::settings_binding::SettingsBinding>,
        own: Option<&str>,
    ) -> Result<ActionResult, ActionError> {
        let channel = context.channel_id;
        let thread =
            self.resolve_thread(channel, binding.map(|b| b.target.as_str()).or(reference))?;
        let server = self.server.as_ref().ok_or(ActionError::MissingAppServer)?;
        let _guard = self.control_lock(&thread.id).await?;
        let generation = server.generation();
        self.validate_lifecycle_binding(binding, channel)?;
        self.archive_preflight(channel, reference, &thread.id, generation, own)
            .await?;
        let children = super::archive_scope::descendants(server, &thread.id, generation).await?;
        let frozen_children = children.clone();
        cdr_app_server::ResidentAppServer::with_archive_stop_scope(
            &thread.id,
            &frozen_children,
            async {
                let mut child_guards = Vec::new();
                for child in &children {
                    child_guards.push(self.control_lock(child).await?);
                    self.archive_preflight(0, Some(child), child, generation, own)
                        .await?;
                }
                if super::archive_scope::descendants(server, &thread.id, generation).await?
                    != children
                {
                    return Err(ActionError::Invalid(
                        "archive descendant scope changed; no archive was sent".into(),
                    ));
                }
                for child in &children {
                    self.archive_preflight(0, Some(child), child, generation, own)
                        .await?;
                }
                self.archive_preflight(channel, reference, &thread.id, generation, own)
                    .await?;
                self.validate_lifecycle_binding(binding, channel)?;
                let mut scope = children;
                scope.insert(thread.id.clone());
                // This transaction commits before awaiting the app-server writer. Late
                // ingress is saved held; existing inputs are preserved without blocking it.
                let reservation = cdr_store::archive_fence::reserve(&self.mirror_db, &scope, own)?;
                *archive_sent = true;
                if let Err(error) = server
                    .execute(archive_thread(&thread.id), Some(generation))
                    .await
                {
                    return Err(dispatch::failure(
                        &self.mirror_db,
                        &reservation,
                        &thread.id,
                        error,
                    ));
                }
                self.verify_archived(&scope).await?;
                cdr_store::archive_fence::verified(&self.mirror_db, &reservation)?;
                if self
                    .bridge_state
                    .selected_thread_id()?
                    .as_ref()
                    .is_some_and(|selected| scope.contains(selected))
                {
                    self.bridge_state.set_selected_thread_id(None)?;
                }
                Ok(immediate(format!(
                    "Archived Codex thread {} ({} conversations, persisted state verified).",
                    thread.id,
                    scope.len()
                )))
            },
        )
        .await?
    }

    async fn archive_preflight(
        &self,
        channel: u64,
        reference: Option<&str>,
        thread: &str,
        generation: u64,
        _own: Option<&str>,
    ) -> Result<(), ActionError> {
        let server = self.server.as_ref().ok_or(ActionError::MissingAppServer)?;
        let lifecycle = server.lifecycle_snapshot().await;
        if !lifecycle.healthy
            || lifecycle.quarantined
            || lifecycle.restart_pending
            || server.generation() != generation
        {
            return Err(ActionError::Invalid(
                "archive connection is unknown or changed; no archive was sent".into(),
            ));
        }
        if reference.is_none() && self.target(channel)?.0 != thread {
            return Err(ActionError::Invalid(
                "archive room target changed; no archive was sent".into(),
            ));
        }
        // Explicit archive takes precedence over active turns and saved bot work.
        // Native thread/archive owns shutdown and writer-lock enforcement.
        if cdr_codex_state::CodexThreadStore::open(&self.state_db)?
            .load_thread(thread, false)?
            .is_none()
        {
            return Err(ActionError::Invalid(
                "archive target is no longer an active stored thread".into(),
            ));
        }
        Ok(())
    }

    async fn verify_archived(&self, scope: &BTreeSet<String>) -> Result<(), ActionError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let store = cdr_codex_state::CodexThreadStore::open(&self.state_db)?;
            let mut unverified = Vec::new();
            for thread in scope {
                if store.load_thread(thread, true)?.is_none() {
                    unverified.push(thread.as_str());
                }
            }
            if unverified.is_empty() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ActionError::Invalid(format!(
                    "archive was acknowledged but persisted state was not verified for [{}]; other scope members may already be archived; do not automatically retry",
                    unverified.join(", ")
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
