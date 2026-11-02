use super::{
    InteractionWorkerError,
    delivery::deliver_interaction_text_idempotent,
    error_disposition::{InteractionErrorDisposition, interaction_error_disposition},
};
use crate::discord_dispatch::{
    InboundInteractionWork,
    delivery_identity::{
        COMPONENT_ERROR_DOMAIN, INTERACTION_ERROR_DOMAIN, component_claim_identity,
        component_delivery_key,
    },
};
use cdr_discord::{http::DiscordHttp, interaction::RoutedWork};
use std::sync::Arc;
use twilight_http::Client;

pub(super) async fn report_interaction_error(
    work: &InboundInteractionWork,
    error: InteractionWorkerError,
    http: Arc<Client>,
) {
    match interaction_error_disposition(&error) {
        InteractionErrorDisposition::IgnoreDuplicate => return,
        InteractionErrorDisposition::LogOnly => {
            eprintln!("interaction_notification_recovery_error: {error}");
            return;
        }
        InteractionErrorDisposition::Report => {}
    }
    let error_text = format!("ERROR: {error}");
    if let RoutedWork::Component(component) = &work.work {
        let claim_identity = component_claim_identity(work.source_message_id, component);
        let logical_key = component_delivery_key(
            work.interaction_id,
            work.source_message_id,
            component,
            claim_identity.as_deref(),
        );
        let delivery = crate::completion_worker::send_recorded_message_with_components(
            &work.custody_database,
            &http,
            work.channel_id,
            &crate::completion_worker::IdempotentChunk {
                domain: COMPONENT_ERROR_DOMAIN,
                logical_key,
                chunk_index: 0,
                content: error_text,
            },
            &[],
        )
        .await;
        if let Err(delivery_error) = delivery {
            eprintln!(
                "ERROR: {error}; additionally failed to report to Discord: {delivery_error:?}"
            );
        }
        return;
    }
    let api = DiscordHttp::new(http, work.application_id)
        .with_store(&work.custody_database, Some(work.channel_id.get()))
        .with_ingress(&work.custody_ingress_id);
    let delivery =
        deliver_interaction_text_idempotent(&api, work, &error_text, INTERACTION_ERROR_DOMAIN)
            .await;
    if let Err(delivery_error) = delivery {
        eprintln!("ERROR: {error}; additionally failed to report to Discord: {delivery_error:?}");
    }
}
