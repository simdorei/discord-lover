use cdr_store::claims::{BusyChoice, NewBusyChoice, create_busy_choice_on_route};
use cdr_store::prompt_intake::admit_busy_queue;

use super::format::INTERVIEW_HEADER;
use super::prompt_intake::PromptAdmission;
use super::queue_result::submission_result;
use super::{ActionContext, ActionError, ActionExecutor, ActionResult, ActionUi, id_i64};
use crate::command_plan::CommandAction;
use crate::component_worker::busy_ready_marker;
use crate::queue_runner::TurnBackend;

const BUSY_CHOICE_TTL_SECONDS: f64 = 1_800.0;

#[cfg(test)]
#[path = "interview_contract.rs"]
mod interview_contract;

impl<B: TurnBackend> ActionExecutor<B> {
    pub(super) async fn execute_prompt_action(
        &self,
        action: CommandAction,
        context: ActionContext,
    ) -> Result<ActionResult, ActionError> {
        match action {
            CommandAction::New { prompt } => self.new_thread(context, &prompt).await,
            CommandAction::Ask { prompt } => {
                self.queue_prompt(
                    context.channel_id,
                    context.user_id,
                    context.discord_message_id,
                    context.auto_queue_when_busy,
                    &prompt,
                )
                .await
            }
            CommandAction::Interview { prompt } => {
                self.queue_prompt(
                    context.channel_id,
                    context.user_id,
                    context.discord_message_id,
                    context.auto_queue_when_busy,
                    &format!("{INTERVIEW_HEADER}{prompt}"),
                )
                .await
            }
            _ => unreachable!("only prompt actions are routed here"),
        }
    }

    pub async fn enqueue_busy_choice(
        &self,
        choice: &BusyChoice,
    ) -> Result<ActionResult, ActionError> {
        let stored_thread_id = choice
            .target_thread_id
            .as_deref()
            .ok_or(ActionError::NoTarget)?;
        let (_, source) = self.current_mirror_target(choice.channel_id, stored_thread_id)?;
        let channel_id = u64::try_from(choice.channel_id).map_err(|_| ActionError::IntegerRange)?;
        let user_id = u64::try_from(choice.owner_user_id).map_err(|_| ActionError::IntegerRange)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs_f64();
        let admitted = admit_busy_queue(
            &self.mirror_db,
            choice,
            stored_thread_id,
            source == "mirror",
            &busy_ready_marker(&choice.choice_id, user_id, channel_id),
            now,
        )?;
        if let Some(intake) = admitted.intake {
            self.process_admitted_prompt(&intake).await
        } else {
            Ok(ActionResult {
                text: format!(
                    "This busy request was already accepted; no duplicate was queued.\njob_id: {}",
                    admitted.job_id
                ),
                waits_for_final: false,
                ui: None,
            })
        }
    }

    pub(super) async fn queue_prompt(
        &self,
        channel_id: u64,
        user_id: u64,
        discord_message_id: Option<u64>,
        auto_queue_when_busy: bool,
        prompt: &str,
    ) -> Result<ActionResult, ActionError> {
        let (thread_id, source) = self.target(channel_id)?;
        if let Some(event) = discord_message_id
            && let Some(ingress) = cdr_store::ingress::by_origin(&self.mirror_db, id_i64(event)?)?
            && let Some(original) = cdr_store::ingress::frozen_slash_target(&ingress)
            && (original != thread_id
                || source != "mirror"
                || ingress.channel_id != id_i64(channel_id)?
                || ingress.owner_user_id != id_i64(user_id)?)
        {
            return Err(ActionError::Invalid(
                "original slash prompt target changed; no request or busy choice was created"
                    .into(),
            ));
        }
        if let Some(message_id) = discord_message_id
            && let Some(submission) = self.queue.replay_submission_for_message(message_id)?
        {
            return Ok(submission_result(
                &thread_id,
                Some(source),
                &submission,
                prompt,
            ));
        }
        let canonical = self.canonicalize_completed_target(&thread_id)?;
        if !auto_queue_when_busy {
            let busy = self.queue.busy_status(&canonical).await?;
            if busy.busy {
                return self
                    .busy_result(
                        &canonical,
                        channel_id,
                        user_id,
                        prompt,
                        busy.allow_steer,
                        source == "mirror",
                    )
                    .await;
            }
        }
        self.admit_prompt(PromptAdmission {
            target_thread_id: &canonical,
            source,
            channel_id,
            user_id,
            discord_message_id,
            auto_queue_when_busy,
            raw_prompt: prompt,
        })
        .await
    }

    pub(super) async fn busy_result(
        &self,
        thread_id: &str,
        channel_id: u64,
        user_id: u64,
        prompt: &str,
        allow_steer: bool,
        mapped: bool,
    ) -> Result<ActionResult, ActionError> {
        let _control = self.control_lock(thread_id).await?;
        let (bound_turn, preceding_job) = self.queue.control_binding(thread_id).await?;
        let unconfirmed_turn = bound_turn.is_none();
        let pro = cdr_pro::prompt::is_pro_command(prompt);
        let (allow_steer, control_status) = if pro {
            (false, Some("Pro requests require connection checks and cannot be steered; choose Queue next to submit this request through Pro validation.".into()))
        } else if allow_steer && bound_turn.is_some() {
            match self
                .verified_control_turn(channel_id, thread_id, bound_turn.as_deref())
                .await
            {
                Ok(_) => (true, None),
                Err(ActionError::Invalid(reason)) => (false, Some(reason)),
                Err(ActionError::MissingAppServer) => {
                    (false, Some(ActionError::MissingAppServer.to_string()))
                }
                Err(error) => return Err(error),
            }
        } else {
            (false, Some("no currently owned active turn is confirmed; controls will be checked again when clicked".into()))
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs_f64();
        let choice_id = create_busy_choice_on_route(
            &self.mirror_db,
            NewBusyChoice {
                owner_user_id: id_i64(user_id)?,
                channel_id: id_i64(channel_id)?,
                target_thread_id: Some(thread_id),
                prompt,
                allow_steer,
                now,
                time_to_live: BUSY_CHOICE_TTL_SECONDS,
            },
            mapped,
        )?;
        cdr_store::control_binding::bind(
            &self.mirror_db,
            &choice_id,
            thread_id,
            bound_turn.as_deref(),
            preceding_job.as_deref(),
        )?;
        let mut text = if unconfirmed_turn {
            format!(
                "Codex has no confirmed active turn for {thread_id}. Choose Queue next to save this request, or Ignore to dismiss it. Steer/Stop needs a confirmed running task."
            )
        } else {
            format!("Codex is busy for {thread_id}. Choose what to do with this request.")
        };
        if let Some(status) = control_status.filter(|_| !unconfirmed_turn) {
            text.push_str("\nControl status: ");
            text.push_str(&status);
        }
        Ok(ActionResult {
            text,
            waits_for_final: false,
            ui: Some(if unconfirmed_turn {
                ActionUi::PendingBusy { choice_id }
            } else if pro {
                ActionUi::ProBusy { choice_id }
            } else {
                ActionUi::Busy {
                    choice_id,
                    allow_steer,
                }
            }),
        })
    }
}
