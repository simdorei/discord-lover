use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use cdr_app_server::{AppServerError, ResidentAppServer};
use cdr_discord::components::ComponentId;
use cdr_discord::http::DiscordHttpError;
use cdr_store::StoreError;
use thiserror::Error;
use twilight_http::Client;

use crate::action_executor::ActionExecutor;
use crate::discord_dispatch::{InboundInteractionWork, InteractionProcessingMode};
use crate::queue_runner::TurnBackend;

mod async_choice;
#[cfg(test)]
mod async_choice_tests;
mod busy;
#[cfg(test)]
mod busy_preflight_tests;
mod confirmation;
mod failure;
mod recovery_abandonment;
mod recovery_publication;
#[cfg(test)]
mod recovery_publication_tests;
mod response;
mod standard;

pub use busy::{BusyChoiceState, BusyComponentError, read_busy_choice_state, validate_busy_choice};
pub use confirmation::{
    ConfirmationClaimState, ConfirmationError, ConfirmationPlan, ConfirmationPlanError,
    busy_confirmation_plan, busy_ready_marker, claim_standard_action, confirmation_ready,
    record_confirmation_ready, send_then_clear, standard_confirmation_plan, standard_ready_marker,
};
pub use failure::{
    ClaimFailureDisposition, action_claim_failure, app_server_claim_failure,
    busy_action_claim_failure, retain_or_release_busy_claim, retain_or_release_component_claim,
};
pub(crate) use response::pending_text_reply_available;
pub use response::{ComponentResponse, build_component_response, handle_pending_text_reply};

pub use confirmation::deliver_confirmation_and_clear;
use standard::prepare_component_action;

const COMPONENT_CLAIM_TTL_SECONDS: f64 = 1_800.0;

pub struct PreparedComponentConfirmation {
    database: std::path::PathBuf,
    channel_id: twilight_model::id::Id<twilight_model::id::marker::ChannelMarker>,
    source_message_id: Option<twilight_model::id::Id<twilight_model::id::marker::MessageMarker>>,
    plan: ConfirmationPlan,
}

impl PreparedComponentConfirmation {
    pub async fn deliver(self, http: Arc<Client>) -> Result<(), ComponentWorkerError> {
        let delivery = deliver_confirmation_and_clear(
            http,
            &self.database,
            self.channel_id,
            self.source_message_id,
            &self.plan,
        );
        if self.plan.domain == recovery_abandonment::CONFIRMATION_DOMAIN {
            tokio::time::timeout(std::time::Duration::from_secs(10), delivery)
                .await
                .map_err(|_| {
                    ComponentWorkerError::AbandonmentNotice(
                        "notification timed out; its receipt must be reconciled".into(),
                    )
                })?
                .map_err(|error| ComponentWorkerError::AbandonmentNotice(error.to_string()))?;
        } else {
            delivery.await?;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ComponentWorkerError {
    #[error(transparent)]
    AsyncAnswerNotDispatched(cdr_store::ingress::AsyncAnswerRefusal),
    #[error("saved-request disposition held: {0}")]
    Abandonment(String),
    #[error(
        "saved-request decision is recorded; notification incomplete, without request replay: {0}"
    )]
    AbandonmentNotice(String),
    #[error("publication consent was not recorded: {0}")]
    PublicationConsent(String),
    #[error("{0}")]
    AsyncQuestion(String),
    #[error("component interaction did not include its source Discord message")]
    MissingSourceMessage,
    #[error("no matching pending app-server request is available")]
    NoPendingRequest,
    #[error(
        "more than one pending app-server request matches; use the exact request's button or displayed [codex-reply:...] prefix"
    )]
    AmbiguousPendingRequest,
    #[error("this legacy approval or input component has expired; use the latest prompt")]
    LegacyComponentExpired,
    #[error("busy-choice component handling is not implemented in this path")]
    BusyChoice,
    #[error("persistent component ID could not be encoded")]
    InvalidComponent,
    #[error(transparent)]
    Authority(#[from] crate::server_prompt_authority::PromptAuthorityError),
    #[error("this approval or input choice was already handled")]
    AlreadyHandled,
    #[error(
        "this component action is claimed, but no durable success marker exists; \
         it may still be in flight or its result is indeterminate"
    )]
    ActionUnconfirmed,
    #[error(
        "component action acceptance is indeterminate; claim retained to prevent duplicate \
         execution: {0}"
    )]
    ActionOutcomeIndeterminate(String),
    #[error(transparent)]
    AppServer(#[from] AppServerError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Discord(#[from] DiscordHttpError),
    #[error(transparent)]
    ConfirmationPlan(#[from] ConfirmationPlanError),
    #[error(transparent)]
    Confirmation(#[from] ConfirmationError),
    #[error("could not clear handled Discord buttons: {0}")]
    Clear(twilight_http::Error),
    #[error(transparent)]
    Busy(#[from] BusyComponentError),
}

impl ComponentWorkerError {
    #[must_use]
    pub(crate) fn action_completed_before_failure(&self) -> bool {
        matches!(
            self,
            Self::Confirmation(ConfirmationError::Recovery(_))
                | Self::Busy(BusyComponentError::Confirmation(
                    ConfirmationError::Recovery(_)
                ))
        )
    }
}

pub async fn handle_component_work<B: TurnBackend>(
    work: &InboundInteractionWork,
    component: &ComponentId,
    executor: &ActionExecutor<B>,
    server: &ResidentAppServer,
) -> Result<PreparedComponentConfirmation, ComponentWorkerError> {
    let plan = if matches!(component, ComponentId::RecoveryAbandonDecision { .. }) {
        recovery_abandonment::handle(work, component, executor).await?
    } else if matches!(component, ComponentId::RecoveryPublicationDecision { .. }) {
        recovery_publication::handle(work, component, executor).await?
    } else if let ComponentId::AsyncChoice {
        question_id,
        option,
    } = component
    {
        async_choice::handle(
            work,
            question_id,
            cdr_store::async_question::Answer::Choice(*option),
            executor,
            server,
        )
        .await?
    } else if let ComponentId::AsyncTextAnswer { question_id, text } = component {
        async_choice::handle(
            work,
            question_id,
            cdr_store::async_question::Answer::Text(text.clone()),
            executor,
            server,
        )
        .await?
    } else if work.processing_mode == InteractionProcessingMode::ConfirmationOnly {
        busy::prepare_confirmation_only(work, component, executor.mirror_db())?
    } else {
        prepare_component_action(work, component, executor, server).await?
    };
    Ok(PreparedComponentConfirmation {
        database: executor.mirror_db().into(),
        channel_id: work.channel_id,
        source_message_id: work.source_message_id,
        plan,
    })
}

pub(super) fn now() -> Result<f64, StoreError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64())
}
