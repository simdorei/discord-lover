use super::{
    INTERACTION_FOLLOWUP_DOMAIN, InteractionWorkerError,
    delivery::deliver_interaction_text_idempotent,
};
use crate::{
    action_executor::{ActionResult, ActionUi},
    discord_dispatch::InboundInteractionWork,
};
use cdr_app_server::ResidentAppServer;
use cdr_discord::{delivery::DeliveryFailure, http::DiscordHttp};
use std::sync::Arc;
use twilight_http::Client;

pub(super) async fn deliver(
    work: &InboundInteractionWork,
    result: &ActionResult,
    database: &std::path::Path,
    server: &ResidentAppServer,
    http: Arc<Client>,
) -> Result<(), InteractionWorkerError> {
    let api = DiscordHttp::new(Arc::clone(&http), work.application_id)
        .with_store(database, Some(work.channel_id.get()))
        .with_ingress(&work.custody_ingress_id);
    if super::new_reply::deliver(work, result, database, &api).await? {
        return Ok(());
    }
    if let Some(ActionUi::ServerPrompts { prompts }) = &result.ui {
        crate::server_prompt_delivery::deliver(
            &crate::server_prompt_delivery::PromptDeliveryContext {
                database,
                server,
                http: &http,
                channel_id: work.channel_id.get(),
                user_id: work.user_id.get(),
                command_key: &work.custody_ingress_id,
            },
            prompts,
        )
        .await?;
        let api = DiscordHttp::new(http, work.application_id)
            .with_store(database, Some(work.channel_id.get()))
            .with_ingress(&work.custody_ingress_id);
        deliver_interaction_text_idempotent(&api, work, &result.text, INTERACTION_FOLLOWUP_DOMAIN)
            .await
            .map_err(InteractionWorkerError::Delivery)?;
        return Ok(());
    }
    let api = DiscordHttp::new(http, work.application_id)
        .with_store(database, Some(work.channel_id.get()))
        .with_ingress(&work.custody_ingress_id);
    let components = crate::action_ui::render_action_ui(result.ui.as_ref())?;
    if components.is_empty() {
        deliver_interaction_text_idempotent(&api, work, &result.text, INTERACTION_FOLLOWUP_DOMAIN)
            .await
            .map_err(InteractionWorkerError::Delivery)?;
    } else {
        api.update_initial_response_with_components(
            &work.interaction_token,
            &result.text,
            &components,
        )
        .await
        .map_err(|source| {
            InteractionWorkerError::Delivery(DeliveryFailure {
                part: 1,
                total_parts: 1,
                attempts: 1,
                source,
            })
        })?;
    }
    Ok(())
}
