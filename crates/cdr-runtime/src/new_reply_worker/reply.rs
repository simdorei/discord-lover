use crate::{
    completion_worker::{CompletionWorkerError, IdempotentChunk, send_recorded_message_chunk},
    message_worker::reply_delivery::{MessageReplyKind, deliver_reply_text},
};
use cdr_discord::http::DiscordHttp;
use cdr_store::{ingress::IngressKind, new_reply::NewReply};
use std::{path::Path, sync::Arc};
use twilight_model::id::Id;

pub(super) async fn recover(
    database: &Path,
    http: &Arc<twilight_http::Client>,
    record: &NewReply,
) -> Result<(), CompletionWorkerError> {
    let id = &record.identity;
    if !record.confirmation_delivered
        && id.kind == IngressKind::Message
        && cdr_store::new_reply::acknowledgement_sendable(database, record)?
    {
        // Same normal sender/domain/key/body as the gateway worker. The shared
        // receipt transaction is the only HTTP claim, even while both race.
        let source = id
            .event_id
            .and_then(|id| u64::try_from(id).ok())
            .and_then(Id::new_checked)
            .ok_or(CompletionWorkerError::ChannelId)?;
        let origin = u64::try_from(id.origin_channel_id)
            .ok()
            .and_then(Id::new_checked)
            .ok_or(CompletionWorkerError::ChannelId)?;
        let api =
            DiscordHttp::new(Arc::clone(http), Id::new(1)).with_store(database, Some(origin.get())); // Channel POST does not use application ID.
        if let Err(error) = deliver_reply_text(
            database,
            &api,
            origin,
            source,
            MessageReplyKind::ActionResult,
            &id.acknowledgement,
        )
        .await
        {
            // Unknown is retained by the common receipt; it is never made retryable.
            eprintln!(
                "new_acknowledgement_not_confirmed job_id={} error={error:?}",
                id.job_id
            );
        }
    }
    if record.warning_due == 1 {
        let origin = u64::try_from(id.origin_channel_id)
            .ok()
            .and_then(Id::new_checked)
            .ok_or(CompletionWorkerError::ChannelId)?;
        send_recorded_message_chunk(
            database,
            http,
            origin,
            &IdempotentChunk {
                domain: "new/verification-notice/v1",
                logical_key: id.job_id.clone(),
                chunk_index: 0,
                content: cdr_store::new_reply::warning_text(record),
            },
        )
        .await?;
    }
    Ok(())
}
