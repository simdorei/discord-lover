use super::{
    Action, AgyError, AgyRouting, Duration, InteractionIngress, InteractionWork, Owner, Result,
    Work,
};
use crate::agy::ingress::{acknowledge, slash_action};
use cdr_discord::{
    gateway::ingress::InteractionIngressTag,
    responses::{autocomplete_response, interaction_message},
};
use futures_util::FutureExt;
use twilight_model::{
    application::interaction::InteractionType,
    channel::message::MessageFlags,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};

#[allow(deprecated, reason = "older Discord interactions use channel_id")]
pub(super) fn channel(item: &InteractionIngress) -> Result<u64> {
    item.event
        .channel
        .as_ref()
        .map(|c| c.id)
        .or(item.event.channel_id)
        .map(twilight_model::id::Id::get)
        .ok_or_else(|| AgyError::Invalid("AGY interaction lacks room".into()))
}

impl AgyRouting {
    pub(super) async fn process_interaction(
        &self,
        item: InteractionWork,
        busy: bool,
    ) -> Result<()> {
        let deadline = item.envelope.received_at + Duration::from_millis(2500);
        if tokio::time::Instant::now() >= deadline {
            return Err(AgyError::Discord(
                "AGY interaction expired without acceptance".into(),
            ));
        }
        let id = channel(&item.envelope)?;
        let mut owner = self.owner(id);
        // Unknown ancestry has its own short budget here, independent of the
        // slow lookup lane. A timeout means an explicit non-acceptance ACK.
        if !busy && owner == Owner::Uncertain {
            owner = match tokio::time::timeout_at(
                deadline.min(tokio::time::Instant::now() + Duration::from_millis(100)),
                std::panic::AssertUnwindSafe(self.classify(id)).catch_unwind(),
            )
            .await
            {
                Ok(Ok(Ok(owner))) => owner,
                Ok(Ok(Err(_)) | Err(_)) => {
                    // Report the local failure without skipping this input's
                    // initial response. No claim or execution is permitted.
                    self.manager.fault(
                        "AGY 방 소유권 확인 실패. 실행하지 않았으며 Codex 처리는 계속됩니다."
                            .into(),
                    );
                    Owner::Uncertain
                }
                Err(_) => {
                    self.classify_later(id);
                    Owner::Uncertain
                }
            };
        }
        if owner.permits_codex() && !busy {
            item.resume
                .try_send(item.envelope)
                .map_err(|_| AgyError::Invalid("Resolved interaction queue full".into()))?;
            return Ok(());
        }
        let event = &item.envelope.event.0;
        if event.kind == InteractionType::ApplicationCommandAutocomplete {
            return acknowledge(
                self.interactions.as_ref(),
                event,
                &autocomplete_response(),
                deadline,
            )
            .await;
        }
        let allowed = self.allowed(
            event.guild_id.map(twilight_model::id::Id::get),
            event.author_id().map(twilight_model::id::Id::get),
        );
        let action = slash_action(event);
        let control = matches!(action, Action::Stop | Action::Status | Action::Help);
        let mut text = if allowed {
            "AGY 요청을 접수할 수 없습니다. 상태를 확인해 주세요."
        } else {
            "이 사용자는 브릿지를 제어할 수 없습니다."
        };
        if let Action::Reject(reason) = action {
            text = reason;
        } else if !busy
            && allowed
            && owner == Owner::Agy
            && item.observed_generation == self.manager.status().generation
            && (control || item.generation.is_some() && item.generation == self.generation())
            && (item.envelope.tag == InteractionIngressTag::Normal || action == Action::Stop)
            && let Some(service) = self.manager.lease()
        {
            let lane = if control { &self.control } else { &self.normal };
            // Reserve completion capacity before promising receipt. The ACK
            // slot is released as soon as the acknowledged work is queued.
            if let Ok(permit) = lane.try_reserve() {
                let response = InteractionResponse {
                    kind: InteractionResponseType::DeferredChannelMessageWithSource,
                    data: Some(InteractionResponseData {
                        flags: Some(MessageFlags::EPHEMERAL),
                        ..Default::default()
                    }),
                };
                acknowledge(self.interactions.as_ref(), event, &response, deadline).await?;
                permit.send(Work::Acknowledged(item, service, action));
                return Ok(());
            }
            text = "AGY가 바쁩니다. 실행 접수되지 않았습니다.";
        }
        if busy {
            text = "AGY가 바쁩니다. 실행 접수되지 않았습니다.";
        }
        acknowledge(
            self.interactions.as_ref(),
            event,
            &interaction_message(text, true),
            deadline,
        )
        .await
    }
}
