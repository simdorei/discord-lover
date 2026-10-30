use std::collections::BTreeSet;

use twilight_model::{
    application::interaction::{Interaction, InteractionData, InteractionType},
    http::interaction::{InteractionResponse, InteractionResponseType},
    id::{
        Id,
        marker::{ChannelMarker, InteractionMarker, MessageMarker, UserMarker},
    },
};

use crate::interaction::{RoutedWork, route_command, route_component, route_modal};
use crate::responses::{interaction_message, invalid_interaction_message};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InteractionAccessPolicy {
    pub allowed_channel_ids: BTreeSet<u64>,
    pub allowed_user_ids: BTreeSet<u64>,
    pub mirrored_channel_ids: BTreeSet<u64>,
    pub allow_all_channels: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoutedInteraction {
    pub interaction_id: Id<InteractionMarker>,
    pub channel_id: Option<Id<ChannelMarker>>,
    pub user_id: Option<Id<UserMarker>>,
    pub source_message_id: Option<Id<MessageMarker>>,
    pub token: String,
    pub initial_response: InteractionResponse,
    pub work: Option<RoutedWork>,
}

#[must_use]
pub fn route_interaction(
    interaction: &Interaction,
    policy: &InteractionAccessPolicy,
    qa_enabled: bool,
) -> RoutedInteraction {
    let channel_id = interaction_channel_id(interaction);
    let user_id = interaction.author_id();
    if !user_allowed(user_id, policy) {
        return envelope(
            interaction,
            channel_id,
            user_id,
            interaction_message("This Discord user is not allowed to control Codex.", true),
            None,
        );
    }
    if !channel_allowed(channel_id, policy) {
        return envelope(
            interaction,
            channel_id,
            user_id,
            interaction_message(
                "This Discord channel is not allowed to control Codex.",
                true,
            ),
            None,
        );
    }
    if interaction.kind == InteractionType::Ping {
        return envelope(
            interaction,
            channel_id,
            user_id,
            InteractionResponse {
                kind: InteractionResponseType::Pong,
                data: None,
            },
            None,
        );
    }
    let route = match interaction.data.as_ref() {
        Some(InteractionData::ApplicationCommand(data)) => {
            route_command(data, interaction.kind, qa_enabled)
        }
        Some(InteractionData::MessageComponent(data)) => route_component(data),
        Some(InteractionData::ModalSubmit(data))
            if interaction.kind == InteractionType::ModalSubmit =>
        {
            route_modal(data)
        }
        Some(_) | None => {
            return envelope(
                interaction,
                channel_id,
                user_id,
                invalid_interaction_message("unsupported or missing interaction data"),
                None,
            );
        }
    };
    match route {
        Ok(route) => envelope(
            interaction,
            channel_id,
            user_id,
            route.initial_response,
            route.work,
        ),
        Err(error) => envelope(
            interaction,
            channel_id,
            user_id,
            invalid_interaction_message(&error.to_string()),
            None,
        ),
    }
}

fn envelope(
    interaction: &Interaction,
    channel_id: Option<Id<ChannelMarker>>,
    user_id: Option<Id<UserMarker>>,
    initial_response: InteractionResponse,
    work: Option<RoutedWork>,
) -> RoutedInteraction {
    RoutedInteraction {
        interaction_id: interaction.id,
        channel_id,
        user_id,
        source_message_id: interaction.message.as_ref().map(|message| message.id),
        token: interaction.token.clone(),
        initial_response,
        work,
    }
}

#[allow(
    deprecated,
    reason = "Discord still sends channel_id while transitioning to channel"
)]
fn interaction_channel_id(interaction: &Interaction) -> Option<Id<ChannelMarker>> {
    interaction
        .channel
        .as_ref()
        .map(|channel| channel.id)
        .or(interaction.channel_id)
}

fn user_allowed(user_id: Option<Id<UserMarker>>, policy: &InteractionAccessPolicy) -> bool {
    policy.allowed_user_ids.is_empty()
        || user_id.is_some_and(|id| policy.allowed_user_ids.contains(&id.get()))
}

fn channel_allowed(
    channel_id: Option<Id<ChannelMarker>>,
    policy: &InteractionAccessPolicy,
) -> bool {
    channel_id.is_some_and(|id| {
        policy.allow_all_channels
            || policy.allowed_channel_ids.contains(&id.get())
            || policy.mirrored_channel_ids.contains(&id.get())
    })
}
