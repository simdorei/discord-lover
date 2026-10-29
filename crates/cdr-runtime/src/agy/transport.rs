use super::{AgyError, Result};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use twilight_http::Client;
use twilight_model::id::Id;
use twilight_model::{
    application::interaction::Interaction, http::interaction::InteractionResponse,
};

pub(super) type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
pub(super) trait Messenger: Send + Sync {
    fn send<'a>(&'a self, channel: u64, key: &'a str, text: &'a str) -> SendFuture<'a>;
}

// Initial acknowledgments have their own bounded lane. Final edits and message
// sends must never retain an acknowledgment slot.
pub(super) trait Interactions: Send + Sync {
    fn acknowledge<'a>(
        &'a self,
        event: &'a Interaction,
        response: &'a InteractionResponse,
    ) -> SendFuture<'a>;
    fn update<'a>(&'a self, event: &'a Interaction, text: &'a str) -> SendFuture<'a>;
}
pub(super) struct DiscordInteractions(pub Arc<Client>);
impl Interactions for DiscordInteractions {
    fn acknowledge<'a>(
        &'a self,
        event: &'a Interaction,
        response: &'a InteractionResponse,
    ) -> SendFuture<'a> {
        Box::pin(async move {
            cdr_discord::http::DiscordHttp::new(Arc::clone(&self.0), event.application_id)
                .acknowledge(event.id, &event.token, response)
                .await
                .map_err(|_| {
                    AgyError::Discord("AGY acknowledgment failed; command not executed".into())
                })
        })
    }
    fn update<'a>(&'a self, event: &'a Interaction, text: &'a str) -> SendFuture<'a> {
        Box::pin(async move {
            tokio::time::timeout(
                Duration::from_secs(15),
                cdr_discord::http::DiscordHttp::new(Arc::clone(&self.0), event.application_id)
                    .update_initial_response(&event.token, text),
            )
            .await
            .map_err(|_| AgyError::Discord("AGY receipt update timed out".into()))?
            .map_err(|_| AgyError::Discord("AGY receipt update failed".into()))?;
            Ok(())
        })
    }
}

pub(super) struct DiscordMessenger(pub Arc<Client>);
impl Messenger for DiscordMessenger {
    fn send<'a>(&'a self, channel: u64, key: &'a str, text: &'a str) -> SendFuture<'a> {
        Box::pin(async move {
            let channel = Id::new_checked(channel)
                .ok_or_else(|| AgyError::Invalid("zero AGY channel id".into()))?;
            for (index, chunk) in cdr_discord::text::split_message(text, 1900)
                .iter()
                .enumerate()
            {
                tokio::time::timeout(
                    Duration::from_secs(15),
                    cdr_discord::idempotent_message::send_idempotent_message_receipt(
                        &self.0, channel, chunk, "agy", key, index,
                    ),
                )
                .await
                .map_err(|_| {
                    AgyError::Discord(
                        "AGY response delivery timed out; result remains in the AGY store".into(),
                    )
                })?
                .map_err(|e| AgyError::Discord(e.to_string()))?;
            }
            Ok(())
        })
    }
}
