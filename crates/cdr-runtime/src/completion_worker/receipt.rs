use super::{CompletionWorkerError, delivery_identity::IdempotentChunk};
use cdr_discord::idempotent_message::{
    IdempotentMessageError, idempotent_message_request_with_components,
};
use cdr_store::delivery_receipt::{self, ReceiptState};
use sha2::{Digest, Sha256};
use twilight_model::channel::{Message, message::Component};
use twilight_model::id::{Id, marker::ChannelMarker};

#[cfg(test)]
#[path = "receipt_http_tests.rs"]
mod http_tests;

pub(crate) async fn send_chunk(
    db: &std::path::Path,
    http: &twilight_http::Client,
    channel: Id<ChannelMarker>,
    chunk: &IdempotentChunk,
) -> Result<(), CompletionWorkerError> {
    send_chunk_with_components(db, http, channel, chunk, &[]).await
}

pub(crate) async fn send_chunk_with_components(
    db: &std::path::Path,
    http: &twilight_http::Client,
    channel: Id<ChannelMarker>,
    chunk: &IdempotentChunk,
    components: &[Component],
) -> Result<(), CompletionWorkerError> {
    send_chunk_guarded(db, http, channel, chunk, components, None).await
}

pub(crate) async fn send_chunk_guarded(
    db: &std::path::Path,
    http: &twilight_http::Client,
    channel: Id<ChannelMarker>,
    chunk: &IdempotentChunk,
    components: &[Component],
    guard: Option<&cdr_store::new_reply::DeliveryGuard<'_>>,
) -> Result<(), CompletionWorkerError> {
    // Validate before recording an attempt: malformed content was never sent.
    let _effect = cdr_store::archived_cleanup::effects::admit(
        db,
        Some(
            i64::try_from(channel.get())
                .map_err(|error| CompletionWorkerError::Delivery(error.to_string()))?,
        ),
        None,
    )?;
    #[cfg(test)]
    let mut timing = crate::test_support::path_timing::PhaseTimer::new("receipt");
    let request = idempotent_message_request_with_components(
        channel,
        &chunk.content,
        components,
        chunk.domain,
        &chunk.logical_key,
        chunk.chunk_index,
    )
    .map_err(|error| CompletionWorkerError::Delivery(error.to_string()))?;
    let key = serde_json::to_string(&(
        channel.get(),
        chunk.domain,
        &chunk.logical_key,
        chunk.chunk_index,
    ))
    .map_err(|error| CompletionWorkerError::Delivery(error.to_string()))?;
    let hash = if components.is_empty() {
        hex::encode(Sha256::digest(chunk.content.as_bytes()))
    } else {
        let payload = serde_json::to_vec(&(&chunk.content, components))
            .map_err(|error| CompletionWorkerError::Delivery(error.to_string()))?;
        hex::encode(Sha256::digest(payload))
    };
    match delivery_receipt::begin_guarded(db,&key,&hash,guard)? {
            ReceiptState::Held(reason) => return Err(CompletionWorkerError::Held(reason)),
            ReceiptState::Delivered(_) => return Ok(()),
            ReceiptState::Unknown => return Err(CompletionWorkerError::Delivery("send outcome unknown; held without automatic resend. Inspect !runners and reconcile the Discord message receipt.".into())),
            ReceiptState::ContentConflict => return Err(CompletionWorkerError::Delivery("delivery content changed for an existing chunk identity; held without sending".into())),
            ReceiptState::RejectedBlocked(reason) => return Err(CompletionWorkerError::Delivery(format!("Discord delivery requires correction; no new request sent: {reason}"))),
            ReceiptState::New => {},
        }
    #[cfg(test)]
    timing.mark("before_http");
    let message = match tokio::time::timeout(std::time::Duration::from_secs(25), async {
        let receipt = http.request::<Message>(request).await?.model().await?;
        Ok::<_, IdempotentMessageError>(receipt.id.to_string())
    })
    .await
    .map_err(|_| {
        CompletionWorkerError::Delivery(
            "Discord send deadline expired; outcome unknown, no automatic resend".into(),
        )
    })? {
        Ok(message) => message,
        Err(error) => {
            if delivery_receipt::confirmed_message(db, &key, &hash)?.is_some() {
                return Ok(());
            }
            if definitely_rejected(&error) {
                if retryable_rejection(&error) {
                    delivery_receipt::release_rejected(db, &key)?;
                } else {
                    delivery_receipt::block_rejected(db, &key, &error.to_string())?;
                }
                return Err(CompletionWorkerError::Delivery(format!(
                    "Discord rejected message; definite rejection recorded separately from unknown delivery: {error}"
                )));
            }
            return Err(CompletionWorkerError::Delivery(format!(
                "send outcome unconfirmed; automatic resend held: {error}"
            )));
        }
    };
    #[cfg(test)]
    timing.mark("http_returned");
    if !delivery_receipt::confirm(db, &key, &message)? {
        return Err(CompletionWorkerError::Delivery("Discord accepted message but its receipt could not be committed; automatic resend held".into()));
    }
    eprintln!(
        "completion_chunk_sent channel_id={channel} message_id={message} chunk={}",
        chunk.chunk_index
    );
    Ok(())
}

fn retryable_rejection(error: &IdempotentMessageError) -> bool {
    matches!(error, IdempotentMessageError::Request(error)
        if matches!(error.kind(), twilight_http::error::ErrorType::Response { status, .. } if status.get() == 429))
}

fn definitely_rejected(error: &IdempotentMessageError) -> bool {
    use twilight_http::error::ErrorType;
    match error {
        IdempotentMessageError::ContentEmpty | IdempotentMessageError::ContentTooLong { .. } => {
            true
        }
        IdempotentMessageError::Request(error) => match error.kind() {
            ErrorType::BuildingRequest
            | ErrorType::CreatingHeader { .. }
            | ErrorType::Json
            | ErrorType::Unauthorized
            | ErrorType::Validation => true,
            ErrorType::Response { status, .. } => matches!(
                status.get(),
                400 | 401 | 403 | 404 | 405 | 413 | 415 | 422 | 429
            ),
            _ => false,
        },
        IdempotentMessageError::Receipt(_) => false,
    }
}
