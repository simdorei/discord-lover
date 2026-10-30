use cdr_discord::components::deferred_update;
use cdr_discord::interaction::RoutedWork;
use tokio::sync::mpsc;
use tokio::time::Instant;
use twilight_model::http::interaction::InteractionResponse;
use twilight_model::id::{
    Id,
    marker::{ApplicationMarker, ChannelMarker, InteractionMarker, MessageMarker, UserMarker},
};

use super::claims::InteractionClaim;
use super::custody::CanonicalRepeat;
use super::dispatch_response::commit_claim;
use super::{
    DiscordDispatchError, DispatchOutcome, InboundInteractionWork, InteractionDispatcher,
    InteractionProcessingMode, InteractionTransport,
};
use crate::restart_readiness::drain::AdmissionPermit;

pub(super) struct CanonicalInteraction {
    pub application_id: Id<ApplicationMarker>,
    pub interaction_id: Id<InteractionMarker>,
    pub channel_id: Id<ChannelMarker>,
    pub user_id: Id<UserMarker>,
    pub source_message_id: Option<Id<MessageMarker>>,
    pub token: String,
    pub work: RoutedWork,
    pub response: InteractionResponse,
    pub claim: InteractionClaim,
    pub custody: CanonicalRepeat,
    pub admission_permit: Option<AdmissionPermit>,
}

impl<T: InteractionTransport> InteractionDispatcher<T> {
    pub(super) async fn dispatch_canonical_repeat(
        &self,
        repeat: CanonicalInteraction,
        deadline: Instant,
    ) -> Result<DispatchOutcome, DiscordDispatchError> {
        if !repeat.custody.confirmation_ready {
            return self.respond_saved_repeat(repeat, deadline).await;
        }
        let reservation = match self.work.clone().try_reserve_owned() {
            Ok(reservation) => reservation,
            Err(mpsc::error::TrySendError::Full(_)) => {
                return self
                    .respond_confirmation_unavailable(repeat, deadline, DispatchOutcome::QueueFull)
                    .await;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return self
                    .respond_confirmation_unavailable(repeat, deadline, DispatchOutcome::Stopping)
                    .await;
            }
        };
        if !self
            .acknowledge_until(
                repeat.interaction_id,
                &repeat.token,
                &repeat.response,
                deadline,
            )
            .await?
        {
            return Ok(DispatchOutcome::DeadlineExceeded);
        }
        commit_claim(repeat.claim)?;
        let receipt = repeat.custody.receipt;
        reservation.send(InboundInteractionWork {
            application_id: repeat.application_id,
            interaction_id: repeat.interaction_id,
            channel_id: repeat.channel_id,
            user_id: repeat.user_id,
            source_message_id: repeat.source_message_id,
            interaction_token: repeat.token,
            work: repeat.work,
            processing_mode: InteractionProcessingMode::ConfirmationOnly,
            custody_database: receipt.database,
            custody_ingress_id: receipt.ingress_id,
            authorized_busy_choice: receipt.busy_choice,
            admission_permit: repeat.admission_permit,
        });
        Ok(DispatchOutcome::Queued)
    }

    async fn respond_saved_repeat(
        &self,
        repeat: CanonicalInteraction,
        deadline: Instant,
    ) -> Result<DispatchOutcome, DiscordDispatchError> {
        // Durable custody already owns this request. A repeat acknowledges only
        // the click, without another notice or another execution attempt.
        let response = deferred_update();
        self.respond_canonical_status(
            repeat,
            deadline,
            &response,
            DispatchOutcome::RespondedWithoutWork,
        )
        .await
    }

    async fn respond_confirmation_unavailable(
        &self,
        repeat: CanonicalInteraction,
        deadline: Instant,
        outcome: DispatchOutcome,
    ) -> Result<DispatchOutcome, DiscordDispatchError> {
        let response = deferred_update();
        self.respond_canonical_status(repeat, deadline, &response, outcome)
            .await
    }

    async fn respond_canonical_status(
        &self,
        repeat: CanonicalInteraction,
        deadline: Instant,
        response: &InteractionResponse,
        outcome: DispatchOutcome,
    ) -> Result<DispatchOutcome, DiscordDispatchError> {
        if !self
            .acknowledge_until(repeat.interaction_id, &repeat.token, response, deadline)
            .await?
        {
            return Ok(DispatchOutcome::DeadlineExceeded);
        }
        commit_claim(repeat.claim)?;
        Ok(outcome)
    }
}
