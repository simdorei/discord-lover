use std::path::Path;
use std::sync::Arc;

use cdr_app_server::ResidentAppServer;
use cdr_discord::components::ComponentError;
use cdr_discord::delivery::DeliveryFailure;
use cdr_discord::http::{DiscordHttp, DiscordHttpError};
use cdr_store::StoreError;
use thiserror::Error;
use twilight_http::Client;
use twilight_model::channel::Message;
use twilight_model::id::{Id, marker::ApplicationMarker};

use crate::action_executor::{ActionError, ActionExecutor};
use crate::command_plan::CommandAction;
use crate::component_worker::{ComponentWorkerError, handle_pending_text_reply};
use crate::config::RuntimeConfig;
use crate::message_plan::{MessagePlan, MessagePlanError};
use crate::queue_runner::TurnBackend;

mod admission;
#[cfg(test)]
mod approval_contract;
#[cfg(test)]
mod bound_text_contract;
mod classification;
mod cleanup_refusal;
#[cfg(test)]
mod cleanup_refusal_tests;
mod custody;
mod discard;
mod execution;
#[cfg(test)]
mod ingress_custody_tests;
#[cfg(test)]
mod new_acceptance_tests;
#[cfg(test)]
pub(crate) mod new_handoff_tests;
#[cfg(test)]
mod new_origin_tests;
#[cfg(test)]
mod pro_busy_contract;
#[cfg(test)]
mod pro_pending_contract;
mod processing_boundary;
mod recovery_abandonment;
pub mod reply_delivery;
#[cfg(test)]
mod reply_receipt_tests;

pub use admission::MessageAdmissionError;
pub(crate) use admission::{AdmittedMessage, admit_message_candidate_at};
pub(crate) use classification::{
    MessageCandidate, MessageClassification, classify_gateway_message,
};
pub(crate) use discard::discard_message_candidate_at;
pub(crate) use processing_boundary::{
    ErrorReportTarget, process_with_error_report, report_processing_error,
};

use admission::MessageProcessingMode;
use execution::{enrich_plan, execute_plan};
use reply_delivery::{MessageReplyKind, deliver_reply_text};

#[derive(Debug, Error)]
pub enum MessageWorkerError {
    #[error(transparent)]
    KnownOutcomeNotification(#[from] crate::cleanup_refusal::NotificationFailure),
    #[error(transparent)]
    PromptDelivery(#[from] crate::server_prompt_delivery::PromptDeliveryError),
    #[error("Codex Discord is restarting. Please retry after restart.")]
    Restarting,
    #[error(transparent)]
    Plan(#[from] MessagePlanError),
    #[error(transparent)]
    Admission(#[from] MessageAdmissionError),
    #[error(transparent)]
    Action(#[from] ActionError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Discord identifier does not fit the SQLite integer contract")]
    IntegerRange,
    #[error("Discord message delivery failed: {0:?}")]
    Delivery(Box<DeliveryFailure<crate::completion_worker::CompletionWorkerError>>),
    #[error(transparent)]
    RecordedDelivery(#[from] crate::completion_worker::CompletionWorkerError),
    #[error(transparent)]
    Component(#[from] ComponentWorkerError),
    #[error(transparent)]
    Attachment(#[from] crate::attachments::AttachmentError),
    #[error(transparent)]
    Ui(#[from] ComponentError),
    #[error(transparent)]
    Discord(#[from] DiscordHttpError),
}

pub struct MessageContext<'a, B: TurnBackend> {
    pub application_id: Id<ApplicationMarker>,
    pub config: &'a RuntimeConfig,
    pub executor: &'a ActionExecutor<B>,
    pub server: &'a ResidentAppServer,
    pub http: Arc<Client>,
    pub attachment_root: &'a Path,
    pub attachment_client: &'a reqwest::Client,
}

impl<'a, B: TurnBackend> MessageContext<'a, B> {
    #[must_use]
    pub fn new(
        application_id: Id<ApplicationMarker>,
        config: &'a RuntimeConfig,
        executor: &'a ActionExecutor<B>,
        server: &'a ResidentAppServer,
        http: Arc<Client>,
        attachment_root: &'a Path,
        attachment_client: &'a reqwest::Client,
    ) -> Self {
        Self {
            application_id,
            config,
            executor,
            server,
            http,
            attachment_root,
            attachment_client,
        }
    }
}

pub(crate) async fn process_admitted_gateway_message<B: TurnBackend>(
    admitted: AdmittedMessage,
    context: &MessageContext<'_, B>,
) -> Result<(), MessageWorkerError> {
    let parts = admitted.into_processing_parts(context.executor.mirror_db())?;
    let mut custody = parts.custody;
    let message = parts.message;
    let channel_id = parts.channel_id;
    let user_id = parts.user_id;
    let plan = parts.frozen_plan?;
    let target = if matches!(
        &plan,
        MessagePlan::Execute(CommandAction::Ask { .. } | CommandAction::Interview { .. })
    ) {
        Some(context.executor.target_thread_id(channel_id)?)
    } else {
        None
    };
    custody.begin(target.as_deref())?;
    let pending_reply = if let MessagePlan::Execute(CommandAction::Ask { prompt }) = &plan
        && !cdr_pro::prompt::is_pro_command(prompt)
    {
        Some(handle_pending_reply(&message, context, channel_id, prompt).await?)
    } else {
        None
    };
    if pending_reply_outcome(parts.processing_mode, pending_reply)? {
        custody.finish()?;
        return Ok(());
    }
    if let MessagePlan::Execute(CommandAction::DiscardRequest { job_id }) = &plan {
        recovery_abandonment::propose(&message, job_id, parts.admission_permit, context).await?;
        custody.finish()?;
        return Ok(());
    }
    let plan = enrich_plan(&message, context, plan).await?;
    let known_refusal = execute_plan(&message, context, channel_id, user_id, plan).await?;
    if let Err(error) = custody.finish() {
        return Err(if known_refusal {
            crate::cleanup_refusal::NotificationFailure::confirmation(
                context.executor.mirror_db(),
                &format!("message:{}", message.id),
                error,
            )
            .into()
        } else {
            error.into()
        });
    }
    context.executor.notify_delivery_ready();
    Ok(())
}

fn pending_reply_outcome(
    mode: MessageProcessingMode,
    handled: Option<bool>,
) -> Result<bool, MessageWorkerError> {
    match handled {
        Some(true) => Ok(true),
        Some(false) | None if mode == MessageProcessingMode::PendingReplyOnly => {
            Err(MessageWorkerError::Restarting)
        }
        Some(false) | None => Ok(false),
    }
}

async fn handle_pending_reply<B: TurnBackend>(
    message: &Message,
    context: &MessageContext<'_, B>,
    channel_id: u64,
    prompt: &str,
) -> Result<bool, MessageWorkerError> {
    let target = context.executor.target_thread_id(channel_id)?;
    let Some(confirmation) = handle_pending_text_reply(
        &target,
        prompt,
        context.server,
        context.executor.mirror_db(),
        channel_id,
        message.author.id.get(),
    )
    .await?
    else {
        return Ok(false);
    };
    cdr_store::ingress::record_result(
        context.executor.mirror_db(),
        &format!("message:{}", message.id),
        &serde_json::json!({"pending_reply":"handled","confirmation":confirmation}),
        custody::now()?,
    )?;
    deliver_reply_text(
        context.executor.mirror_db(),
        &DiscordHttp::new(Arc::clone(&context.http), context.application_id)
            .with_store(context.executor.mirror_db(), Some(message.channel_id.get())),
        message.channel_id,
        message.id,
        MessageReplyKind::PendingConfirmation,
        &confirmation,
    )
    .await
    .map_err(|error| MessageWorkerError::Delivery(Box::new(error)))?;
    Ok(true)
}

#[cfg(test)]
mod drain_tests {
    use super::*;

    #[test]
    fn pending_reply_only_never_falls_through_after_the_request_disappears() {
        assert!(matches!(
            pending_reply_outcome(MessageProcessingMode::PendingReplyOnly, Some(false)),
            Err(MessageWorkerError::Restarting)
        ));
        assert!(matches!(
            pending_reply_outcome(MessageProcessingMode::PendingReplyOnly, None),
            Err(MessageWorkerError::Restarting)
        ));
        assert!(
            pending_reply_outcome(MessageProcessingMode::Normal, Some(false)).is_ok_and(|v| !v)
        );
    }
}
