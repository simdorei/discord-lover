use cdr_discord::gateway::ingress::InteractionIngressTag;
use cdr_discord::interaction::RoutedWork;
use cdr_discord::interaction_access::{RoutedInteraction, route_interaction};
use tokio::time::Instant;
use twilight_model::application::interaction::Interaction;
use twilight_model::http::interaction::InteractionResponse;
use twilight_model::id::{Id, marker::ApplicationMarker};

use super::claims::{InteractionClaim, InteractionClaimAttempt};
use super::custody::{self, StageOutcome, StageRequest};
use super::dispatch_canonical::CanonicalInteraction;
use super::dispatch_queue::StagedInteraction;
use super::dispatch_response::{custody_error, hold_or_log};
use super::{
    DiscordDispatchError, DispatchOutcome, INTERACTION_ACK_BUDGET, InteractionDispatcher,
    InteractionTransport,
};
use crate::discord_dispatch::response::response_for;
use crate::restart_readiness::drain::AdmissionPermit;

impl<T: InteractionTransport> InteractionDispatcher<T> {
    pub async fn dispatch(
        &self,
        interaction: &Interaction,
        received_at: Instant,
        tag: InteractionIngressTag,
    ) -> Result<DispatchOutcome, DiscordDispatchError> {
        let Some(deadline) = received_at.checked_add(INTERACTION_ACK_BUDGET) else {
            return Ok(DispatchOutcome::DeadlineExceeded);
        };
        if Instant::now() >= deadline {
            return Ok(DispatchOutcome::DeadlineExceeded);
        }
        let routed = route_interaction(interaction, &self.policy, self.qa_enabled);
        let (admission_permit, admission_sealed) =
            super::admission::admit(self.admission.as_ref(), routed.work.as_ref())?;
        let autocomplete = matches!(routed.work.as_ref(), Some(RoutedWork::Autocomplete(_)));
        let response = if admission_sealed && !autocomplete {
            cdr_discord::responses::interaction_message(
                "Codex Discord is restarting. Please retry after restart.",
                true,
            )
        } else {
            response_for(
                interaction,
                tag,
                routed.initial_response.clone(),
                routed.work.as_ref(),
                &self.autocomplete,
            )
        };
        if Instant::now() >= deadline {
            return Ok(DispatchOutcome::DeadlineExceeded);
        }
        let claim = match self.claims.try_claim(routed.interaction_id) {
            InteractionClaimAttempt::Claimed(claim) => claim,
            InteractionClaimAttempt::DuplicatePending => {
                return Ok(DispatchOutcome::DuplicatePending);
            }
            InteractionClaimAttempt::DuplicateCommitted => return Ok(DispatchOutcome::Duplicate),
            InteractionClaimAttempt::Saturated => {
                return Err(DiscordDispatchError::ClaimCacheSaturated);
            }
        };
        if Instant::now() >= deadline {
            return Ok(DispatchOutcome::DeadlineExceeded);
        }
        if !admission_sealed
            && tag == InteractionIngressTag::Normal
            && let Some(modal) = super::async_modal::response(&self.ingress_db, &routed)
        {
            if !self
                .acknowledge_until(routed.interaction_id, &routed.token, &modal, deadline)
                .await?
            {
                return Ok(DispatchOutcome::DeadlineExceeded);
            }
            super::dispatch_response::commit_claim(claim)?;
            return Ok(DispatchOutcome::RespondedWithoutWork);
        }
        let executable = !admission_sealed
            && tag == InteractionIngressTag::Normal
            && routed
                .work
                .as_ref()
                .is_some_and(|work| !matches!(work, RoutedWork::Autocomplete(_)));
        if !executable {
            if !self
                .acknowledge_until(routed.interaction_id, &routed.token, &response, deadline)
                .await?
            {
                return Ok(DispatchOutcome::DeadlineExceeded);
            }
            if !claim.commit() {
                return Err(DiscordDispatchError::ClaimState);
            }
            return Ok(if admission_sealed {
                DispatchOutcome::Stopping
            } else {
                DispatchOutcome::RespondedWithoutWork
            });
        }
        self.dispatch_executable(
            interaction.application_id,
            routed,
            response,
            deadline,
            claim,
            admission_permit,
        )
        .await
    }

    async fn dispatch_executable(
        &self,
        application_id: Id<ApplicationMarker>,
        routed: RoutedInteraction,
        response: InteractionResponse,
        deadline: Instant,
        claim: InteractionClaim,
        admission_permit: Option<AdmissionPermit>,
    ) -> Result<DispatchOutcome, DiscordDispatchError> {
        let (Some(channel_id), Some(user_id)) = (routed.channel_id, routed.user_id) else {
            return self
                .reject_missing_identity(routed.interaction_id, &routed.token, deadline, claim)
                .await;
        };
        let work = routed.work.expect("executable interaction has routed work");
        let mut custody = match custody::stage(
            &self.ingress_db,
            &StageRequest {
                application_id: application_id.get(),
                interaction_id: routed.interaction_id.get(),
                channel_id: channel_id.get(),
                user_id: user_id.get(),
                source_message_id: routed.source_message_id.map(Id::get),
                work: &work,
                settings_resolver: self.settings_resolver.as_ref(),
            },
        )
        .map_err(|error| custody_error(&error))?
        {
            StageOutcome::Created(custody) => custody,
            StageOutcome::Duplicate => return Ok(DispatchOutcome::Duplicate),
            StageOutcome::CanonicalRepeat(custody) => {
                return self
                    .dispatch_canonical_repeat(
                        CanonicalInteraction {
                            application_id,
                            interaction_id: routed.interaction_id,
                            channel_id,
                            user_id,
                            source_message_id: routed.source_message_id,
                            token: routed.token,
                            work,
                            response,
                            claim,
                            custody,
                            admission_permit,
                        },
                        deadline,
                    )
                    .await;
            }
            StageOutcome::BusyChoiceUnavailable => {
                return self
                    .reject_busy_choice_unavailable(
                        routed.interaction_id,
                        &routed.token,
                        deadline,
                        claim,
                    )
                    .await;
            }
        };
        if Instant::now() >= deadline {
            hold_or_log(&mut custody, "discord_ack_deadline");
            return Ok(DispatchOutcome::DeadlineExceeded);
        }
        self.queue_staged(
            StagedInteraction {
                application_id,
                interaction_id: routed.interaction_id,
                channel_id,
                user_id,
                source_message_id: routed.source_message_id,
                token: routed.token,
                work,
                claim,
                custody,
                admission_permit,
            },
            response,
            deadline,
        )
        .await
    }
}
