use std::sync::Arc;

use thiserror::Error;
use twilight_http::{
    Client,
    request::Request,
    response::{DeserializeBodyError, marker::ListBody},
    routing::Route,
};
use twilight_model::{
    channel::{
        Message,
        message::{AllowedMentions, Component},
    },
    http::interaction::InteractionResponse,
    id::{
        Id,
        marker::{ApplicationMarker, ChannelMarker, GuildMarker, InteractionMarker},
    },
};

use crate::commands::slash_commands;
use crate::idempotent_message::{IdempotentMessageError, send_idempotent_message_with_components};

const HISTORY_MESSAGE_LIMIT: u16 = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandRegistrationScope {
    Global,
    Guild(Id<GuildMarker>),
}

#[derive(Debug, Error)]
pub enum DiscordHttpError {
    #[error("Discord effect held: {0}")]
    Held(String),
    #[error("Discord operation deadline expired; outcome unknown, no automatic retry")]
    Deadline,
    #[error("Discord HTTP request failed: {0}")]
    Request(#[from] twilight_http::Error),
    #[error("Discord response model decode failed: {0}")]
    Model(#[from] DeserializeBodyError),
    #[error(transparent)]
    Idempotent(#[from] IdempotentMessageError),
}

#[derive(Clone)]
pub struct DiscordHttp {
    client: Arc<Client>,
    application_id: Id<ApplicationMarker>,
    store: Option<(std::path::PathBuf, Option<i64>)>,
    ingress: Option<String>,
}

impl DiscordHttp {
    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }

    #[must_use]
    pub const fn new(client: Arc<Client>, application_id: Id<ApplicationMarker>) -> Self {
        Self {
            client,
            application_id,
            store: None,
            ingress: None,
        }
    }

    #[must_use]
    pub fn with_store(mut self, path: &std::path::Path, channel: Option<u64>) -> Self {
        self.store = Some((path.to_owned(), channel.and_then(|v| i64::try_from(v).ok())));
        self
    }

    #[must_use]
    pub fn with_ingress(mut self, key: &str) -> Self {
        self.ingress = Some(key.into());
        self
    }

    fn admit(
        &self,
        channel: Option<u64>,
    ) -> Result<Option<cdr_store::archived_cleanup::effects::Permit>, DiscordHttpError> {
        let Some((db, default_channel)) = &self.store else {
            return Ok(None);
        };
        let channel = channel
            .and_then(|v| i64::try_from(v).ok())
            .or(*default_channel)
            .ok_or_else(|| DiscordHttpError::Held("missing delivery room identity".into()))?;
        cdr_store::archived_cleanup::effects::admit_record(
            db,
            Some(channel),
            None,
            self.ingress
                .as_deref()
                .map(|key| ("discord_ingress_journal", key)),
        )
        .map(Some)
        .map_err(|e| DiscordHttpError::Held(e.to_string()))
    }

    pub async fn register_slash_commands(
        &self,
        scope: CommandRegistrationScope,
        qa_enabled: bool,
    ) -> Result<(), DiscordHttpError> {
        let commands = slash_commands(qa_enabled);
        let interactions = self.client.interaction(self.application_id);
        match scope {
            CommandRegistrationScope::Global => {
                interactions.set_global_commands(&commands).await?;
            }
            CommandRegistrationScope::Guild(guild_id) => {
                interactions.set_guild_commands(guild_id, &commands).await?;
            }
        }
        Ok(())
    }

    pub async fn acknowledge(
        &self,
        interaction_id: Id<InteractionMarker>,
        interaction_token: &str,
        response: &InteractionResponse,
    ) -> Result<(), DiscordHttpError> {
        let _permit = self.admit(None)?;
        tokio::time::timeout(
            std::time::Duration::from_secs(25),
            self.client
                .interaction(self.application_id)
                .create_response(interaction_id, interaction_token, response),
        )
        .await
        .map_err(|_| DiscordHttpError::Deadline)??;
        Ok(())
    }

    /// Fetch Discord's newest bounded page for one channel.
    pub async fn fetch_latest_channel_messages(
        &self,
        channel_id: Id<ChannelMarker>,
    ) -> Result<Vec<Message>, DiscordHttpError> {
        let request = latest_channel_messages_request(channel_id);
        let response = self.client.request::<ListBody<Message>>(request).await?;

        Ok(response.models().await?)
    }

    pub async fn update_initial_response(
        &self,
        interaction_token: &str,
        content: &str,
    ) -> Result<(), DiscordHttpError> {
        self.update_initial_response_with_components(interaction_token, content, &[])
            .await
    }

    pub async fn update_initial_response_with_components(
        &self,
        interaction_token: &str,
        content: &str,
        components: &[Component],
    ) -> Result<(), DiscordHttpError> {
        let _permit = self.admit(None)?;
        let allowed_mentions = AllowedMentions::default();
        let interaction = self.client.interaction(self.application_id);
        let mut request = interaction
            .update_response(interaction_token)
            .allowed_mentions(Some(&allowed_mentions))
            .content(Some(content));
        if !components.is_empty() {
            request = request.components(Some(components));
        }
        tokio::time::timeout(std::time::Duration::from_secs(25), request)
            .await
            .map_err(|_| DiscordHttpError::Deadline)??;
        Ok(())
    }

    /// Create one Discord message with a stable, server-enforced nonce.
    pub async fn send_idempotent_message(
        &self,
        channel_id: Id<ChannelMarker>,
        content: &str,
        components: &[Component],
        domain: &str,
        logical_key: &str,
        chunk_index: usize,
    ) -> Result<(), DiscordHttpError> {
        let _permit = self.admit(Some(channel_id.get()))?;
        tokio::time::timeout(
            std::time::Duration::from_secs(25),
            send_idempotent_message_with_components(
                &self.client,
                channel_id,
                content,
                components,
                domain,
                logical_key,
                chunk_index,
            ),
        )
        .await
        .map_err(|_| DiscordHttpError::Deadline)??;
        Ok(())
    }
}

fn latest_channel_messages_request(channel_id: Id<ChannelMarker>) -> Request {
    Request::from_route(&Route::GetMessages {
        after: None,
        around: None,
        before: None,
        channel_id: channel_id.get(),
        limit: Some(HISTORY_MESSAGE_LIMIT),
    })
}

#[cfg(test)]
mod tests {
    use super::{DiscordHttpError, HISTORY_MESSAGE_LIMIT, latest_channel_messages_request};
    use twilight_http::{request::Method, response::DeserializeBodyError};
    use twilight_model::id::{Id, marker::ChannelMarker};

    #[test]
    fn latest_history_request_is_one_unpaginated_page_of_ten() {
        let request = latest_channel_messages_request(Id::<ChannelMarker>::new(42));

        assert_eq!(HISTORY_MESSAGE_LIMIT, 10);
        assert_eq!(request.method(), Method::Get);
        assert_eq!(request.path(), "channels/42/messages?limit=10");
        assert!(request.body().is_none());
    }

    #[test]
    fn twilight_model_decode_errors_are_not_collapsed_into_http_errors() {
        fn map_model_error(error: DeserializeBodyError) -> DiscordHttpError {
            DiscordHttpError::from(error)
        }

        let _: fn(DeserializeBodyError) -> DiscordHttpError = map_model_error;
    }
}
