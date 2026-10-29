use super::{AgyError, AgyRuntime, Result};
use std::sync::Arc;
use twilight_model::id::Id;

pub(super) type HistoryFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Vec<twilight_model::channel::Message>>> + Send + 'a,
    >,
>;
pub(super) trait History: Send + Sync {
    fn recent(&self, channel: u64) -> HistoryFuture<'_>;
}
pub(super) struct DiscordHistory(pub Arc<twilight_http::Client>);
impl History for DiscordHistory {
    fn recent(&self, channel: u64) -> HistoryFuture<'_> {
        Box::pin(async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                self.0
                    .channel_messages(Id::new(channel))
                    .limit(10)
                    .await
                    .map_err(|_| AgyError::Discord("AGY history request failed".into()))?
                    .models()
                    .await
                    .map_err(|_| AgyError::Discord("AGY history payload invalid".into()))
            })
            .await
            .map_err(|_| AgyError::Discord("AGY history request timed out".into()))?
        })
    }
}

impl AgyRuntime {
    /// Recover a bounded Discord gateway overflow through the same AGY admission
    /// and durable request IDs. Never hand an AGY gap to Codex's history adapter.
    pub(crate) async fn recover_gap(
        self: &Arc<Self>,
        channel: u64,
        floor: u64,
        bot: u64,
    ) -> Result<bool> {
        let mut messages = self.history.recent(channel).await?;
        let reached = messages.len() < 10 || messages.iter().any(|m| m.id.get() <= floor);
        messages.sort_by_key(|m| m.id);
        for mut message in messages.into_iter().filter(|m| m.id.get() >= floor) {
            // Guild message history does not include guild_id in every payload.
            message.guild_id = Some(Id::new(self.guild));
            self.handle_message(&message, Some(bot)).await?;
        }
        if !reached {
            eprintln!(
                "agy_history_gap_incomplete channel={channel} floor={floor}; no Codex replay"
            );
        }
        Ok(reached)
    }
}
