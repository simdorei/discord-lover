use super::{ExecutionCustody, InteractionWorkerError};
use crate::discord_dispatch::{
    InboundInteractionWork,
    delivery_identity::{COMPONENT_ERROR_DOMAIN, component_claim_identity, component_delivery_key},
};
use cdr_discord::interaction::RoutedWork;
use cdr_store::ingress::AsyncAnswerRefusal;
use std::sync::Arc;
use twilight_http::Client;

pub(super) async fn deliver(
    work: &InboundInteractionWork,
    custody: &mut ExecutionCustody,
    http: &Arc<Client>,
    refusal: &AsyncAnswerRefusal,
) -> Result<bool, InteractionWorkerError> {
    custody.record_result(&refusal.outcome())?;
    let RoutedWork::Component(component) = &work.work else {
        return Err(InteractionWorkerError::Unsupported(
            "non-component answer refusal",
        ));
    };
    let claim = component_claim_identity(work.source_message_id, component);
    let key = component_delivery_key(
        work.interaction_id,
        work.source_message_id,
        component,
        claim.as_deref(),
    );
    crate::completion_worker::send_recorded_message_with_components(
        &work.custody_database,
        http,
        work.channel_id,
        &crate::completion_worker::IdempotentChunk {
            domain: COMPONENT_ERROR_DOMAIN,
            logical_key: key,
            chunk_index: 0,
            content: refusal.to_string(),
        },
        &[],
    )
    .await
    .map_err(|error| InteractionWorkerError::AsyncAnswerNotice(error.to_string()))?;
    Ok(false)
}
