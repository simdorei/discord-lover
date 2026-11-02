mod attempt;
mod first_reply;
mod journal;
mod project;

use cdr_app_server::{extract_thread_id, requests::start_thread};
use cdr_store::ingress::{StoredIngress, begin_thread_start, record_created_thread};
use cdr_store::prompt_intake::{NewPromptIntake, get_prompt_intake};
use uuid::Uuid;

use super::queue_result::submission_result;
use super::{ActionContext, ActionError, ActionExecutor, ActionResult, id_i64};
use crate::queue_runner::TurnBackend;
use journal::unix_now;

impl<B: TurnBackend> ActionExecutor<B> {
    pub(super) async fn new_thread(
        &self,
        context: ActionContext,
        prompt: &str,
    ) -> Result<ActionResult, ActionError> {
        let ingress = self.admit_new_thread(context, prompt)?;
        if let Some(result) = self.replay_new_thread_owner(&ingress).await? {
            return self.new_first_reply(&ingress, result);
        }
        let execution_prompt =
            cdr_store::ingress::new_execution_prompt(&ingress)?.ok_or_else(|| {
                ActionError::Invalid("new request has no original execution input".into())
            })?;
        let Some(server) = self.server.as_ref() else {
            return Err(self.hold_new_thread(&ingress, &ActionError::MissingAppServer, true));
        };
        let generation = server.generation();
        let stored_generation = id_i64(generation)?;
        // A new thread has no target ID yet. Its original Discord room still
        // participates in retirement admission and bounded dispatch draining.
        let start_effect = cdr_store::archived_cleanup::effects::admit(
            &self.mirror_db,
            Some(ingress.channel_id),
            ingress.target_thread_id.as_deref(),
        )?;
        if !begin_thread_start(
            &self.mirror_db,
            &ingress.ingress_id,
            stored_generation,
            unix_now()?,
        )? {
            // A duplicate does not own this attempt. In particular it must not
            // place the concurrently executing original request on manual hold.
            return Err(ActionError::Invalid(format!(
                "thread/start was already attempted for request {}; its existing state is preserved and no duplicate was started",
                ingress.ingress_id
            )));
        }
        let _attempt = attempt::AttemptGuard::new(&self.mirror_db, &ingress.ingress_id);
        let cwd = self.freeze_new_context(&ingress, context, stored_generation)?;
        let value = match server
            .execute(start_thread(cwd.as_deref()), Some(generation))
            .await
        {
            Ok(value) => value,
            Err(error) => return Err(self.hold_new_thread(&ingress, &error.into(), false)),
        };
        let Some(thread_id) = extract_thread_id(&value) else {
            return Err(self.hold_new_thread(
                &ingress,
                &ActionError::Invalid("thread/start returned no thread id".into()),
                false,
            ));
        };
        self.queue
            .backend
            .remember_new_thread(&thread_id, generation);
        record_created_thread(
            &self.mirror_db,
            &ingress.ingress_id,
            stored_generation,
            &thread_id,
            unix_now()?,
        )
        .map_err(|error| {
            self.hold_new_thread(&ingress,&ActionError::Invalid(format!(
                "thread {thread_id} was created but recording its known identity failed: {error}"
            )),false)
        })?;
        drop(start_effect);
        // The original prompt and returned ID are durable before remote room creation.
        // An uncertain Discord create is held, never blindly retried or sent elsewhere.
        let destination = if let Some(sync) = self.mirror_sync.get() {
            sync.link_new_thread(context.channel_id, &thread_id, prompt, cwd.as_deref())
                .await
                .map_err(|error| self.hold_new_thread(&ingress, &error.into(), false))?
        } else {
            context.channel_id // Headless executor: no Discord transport was configured.
        };
        let admitted = self.admit_new_prompt(
            NewPromptIntake {
                job_id: &Uuid::new_v4().to_string(),
                target_thread_id: &thread_id,
                channel_id: id_i64(destination)?,
                owner_user_id: Some(id_i64(context.user_id)?),
                discord_message_id: context.discord_message_id.map(id_i64).transpose()?,
                raw_prompt: execution_prompt,
                auto_queue_when_busy: context.auto_queue_when_busy,
                require_current_mirror: self.mirror_sync.get().is_some(),
                created_at: unix_now()?,
            },
            &ingress,
            stored_generation,
        )
        .map_err(|error| {
            self.hold_new_thread(
                &ingress,
                &ActionError::Invalid(format!(
                    "thread {thread_id} was created but its prompt ownership could not be saved: {error}"
                )),
                false,
            )
        })?;
        self.bridge_state.set_selected_thread_id(Some(&thread_id))?;
        let result = self.process_admitted_prompt(&admitted.intake).await?;
        self.new_first_reply(&ingress, result)
    }

    async fn replay_new_thread_owner(
        &self,
        ingress: &StoredIngress,
    ) -> Result<Option<ActionResult>, ActionError> {
        let Some(job_id) = ingress.owner_id.as_deref() else {
            return Ok(None);
        };
        if let Some(intake) = get_prompt_intake(&self.mirror_db, job_id)? {
            return self.process_admitted_prompt(&intake).await.map(Some);
        }
        if let Some((target, submission)) =
            self.queue.replay_submission_with_target_for_job(job_id)?
        {
            let prompt = cdr_store::ingress::new_command_prompt(ingress).ok_or_else(|| {
                ActionError::Invalid("saved new request has no verified original prompt".into())
            })?;
            return Ok(Some(submission_result(
                &target,
                Some("new"),
                &submission,
                prompt,
            )));
        }
        Ok(Some(ActionResult {
            text: format!(
                "This /new request was already accepted; no new thread or prompt was created.\nthread_id: {}\njob_id: {job_id}",
                ingress.target_thread_id.as_deref().unwrap_or("recorded")
            ),
            waits_for_final: false,
            ui: None,
        }))
    }
}
