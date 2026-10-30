pub const AUTO_RESERVE_REMOVED: &str = "자동 Reserve 전환 기능은 제거되었습니다. 설정은 변경하지 않았습니다. 필요하면 !settings --model gpt-reserve 로 직접 선택하세요.";

use std::collections::{BTreeMap, HashSet};

use serde::Serialize;
use thiserror::Error;
use twilight_model::{
    application::{
        command::{CommandOption, CommandType},
        interaction::{
            InteractionType,
            application_command::{CommandData, CommandOptionValue},
            message_component::MessageComponentInteractionData,
        },
    },
    http::interaction::{InteractionResponse, InteractionResponseType},
};

use crate::responses::autocomplete_response;
mod modal;
use crate::{
    commands::slash_commands,
    components::{ComponentId, parse_component_id},
};
pub use modal::route_modal;

#[derive(Clone, Debug, PartialEq)]
pub struct InteractionRoute {
    pub initial_response: InteractionResponse,
    pub work: Option<RoutedWork>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum RoutedWork {
    Slash(SlashInvocation),
    Autocomplete(AutocompleteInvocation),
    Component(ComponentId),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SlashInvocation {
    pub name: String,
    values: BTreeMap<String, SlashValue>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AutocompleteInvocation {
    pub command_name: String,
    pub option_name: String,
    pub current: String,
    pub selected_model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
enum SlashValue {
    Boolean(bool),
    Integer(i64),
    String(String),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum InteractionRouteError {
    #[error("{0}")]
    RemovedFeature(&'static str),
    #[error("unsupported Discord interaction type")]
    WrongInteractionType,
    #[error("only slash commands are supported")]
    WrongCommandType,
    #[error("unknown Discord slash command: {0}")]
    UnknownCommand(String),
    #[error("unknown or invalid option {option} for slash command {command}")]
    InvalidOption { command: String, option: String },
    #[error("duplicate option {option} for slash command {command}")]
    DuplicateOption { command: String, option: String },
    #[error("required option {option} is missing for slash command {command}")]
    MissingOption { command: String, option: String },
    #[error("autocomplete interaction has no valid focused option")]
    MissingFocusedOption,
    #[error("Discord component custom ID is unknown or invalid")]
    UnknownComponent,
}

impl SlashInvocation {
    #[must_use]
    pub fn has_option(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    #[must_use]
    pub fn string(&self, name: &str) -> Option<&str> {
        match self.values.get(name) {
            Some(SlashValue::String(value)) => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub fn integer(&self, name: &str) -> Option<i64> {
        match self.values.get(name) {
            Some(SlashValue::Integer(value)) => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub fn boolean(&self, name: &str) -> Option<bool> {
        match self.values.get(name) {
            Some(SlashValue::Boolean(value)) => Some(*value),
            _ => None,
        }
    }
}

pub fn route_command(
    data: &CommandData,
    interaction_type: InteractionType,
    qa_enabled: bool,
) -> Result<InteractionRoute, InteractionRouteError> {
    if !matches!(
        interaction_type,
        InteractionType::ApplicationCommand | InteractionType::ApplicationCommandAutocomplete
    ) {
        return Err(InteractionRouteError::WrongInteractionType);
    }
    if data.kind != CommandType::ChatInput {
        return Err(InteractionRouteError::WrongCommandType);
    }
    if data.name == "settings" && data.options.iter().any(|o| o.name == "auto_reserve") {
        return Err(InteractionRouteError::RemovedFeature(AUTO_RESERVE_REMOVED));
    }
    let expected = slash_commands(qa_enabled)
        .into_iter()
        .find(|command| command.name == data.name)
        .ok_or_else(|| InteractionRouteError::UnknownCommand(data.name.clone()))?;
    let autocomplete = interaction_type == InteractionType::ApplicationCommandAutocomplete;
    let (values, focused) =
        validate_options(&data.name, &data.options, &expected.options, autocomplete)?;
    let (initial_response, work) = if autocomplete {
        let focused = focused.ok_or(InteractionRouteError::MissingFocusedOption)?;
        (
            autocomplete_response(),
            RoutedWork::Autocomplete(AutocompleteInvocation {
                command_name: data.name.clone(),
                option_name: focused.0,
                current: focused.1,
                selected_model: match values.get("model") {
                    Some(SlashValue::String(value)) => Some(value.clone()),
                    _ => None,
                },
            }),
        )
    } else {
        (
            deferred_channel_response(),
            RoutedWork::Slash(SlashInvocation {
                name: data.name.clone(),
                values,
            }),
        )
    };
    Ok(InteractionRoute {
        initial_response,
        work: Some(work),
    })
}

pub fn route_component(
    data: &MessageComponentInteractionData,
) -> Result<InteractionRoute, InteractionRouteError> {
    let component = if let Some(question) = data.custom_id.strip_prefix("codex_async_select:") {
        let [value] = data.values.as_slice() else {
            return Err(InteractionRouteError::UnknownComponent);
        };
        parse_component_id(&format!("codex_async:{question}:{value}"))
    } else {
        parse_component_id(&data.custom_id)
    }
    .ok_or(InteractionRouteError::UnknownComponent)?;
    let initial_response = if let ComponentId::AsyncTextInput { question_id } = &component {
        modal::open(question_id)?
    } else {
        crate::components::deferred_update()
    };
    Ok(InteractionRoute {
        initial_response,
        work: Some(RoutedWork::Component(component)),
    })
}

type FocusedOption = (String, String);

fn validate_options(
    command: &str,
    actual: &[twilight_model::application::interaction::application_command::CommandDataOption],
    expected: &[CommandOption],
    autocomplete: bool,
) -> Result<(BTreeMap<String, SlashValue>, Option<FocusedOption>), InteractionRouteError> {
    let mut names = HashSet::new();
    let mut values = BTreeMap::new();
    let mut focused = None;
    for option in actual {
        if !names.insert(option.name.as_str()) {
            return Err(duplicate(command, &option.name));
        }
        let schema = expected
            .iter()
            .find(|candidate| candidate.name == option.name)
            .ok_or_else(|| invalid(command, &option.name))?;
        if let CommandOptionValue::Focused(current, kind) = &option.value {
            if !autocomplete || schema.autocomplete != Some(true) || schema.kind != *kind {
                return Err(invalid(command, &option.name));
            }
            if focused
                .replace((option.name.clone(), current.clone()))
                .is_some()
            {
                return Err(InteractionRouteError::MissingFocusedOption);
            }
        } else {
            let value = slash_value(&option.value).ok_or_else(|| invalid(command, &option.name))?;
            if schema.kind != option.value.kind() {
                return Err(invalid(command, &option.name));
            }
            values.insert(option.name.clone(), value);
        }
    }
    for option in expected
        .iter()
        .filter(|option| option.required == Some(true))
    {
        if !names.contains(option.name.as_str()) {
            return Err(InteractionRouteError::MissingOption {
                command: command.into(),
                option: option.name.clone(),
            });
        }
    }
    Ok((values, focused))
}

fn slash_value(value: &CommandOptionValue) -> Option<SlashValue> {
    match value {
        CommandOptionValue::Boolean(value) => Some(SlashValue::Boolean(*value)),
        CommandOptionValue::Integer(value) => Some(SlashValue::Integer(*value)),
        CommandOptionValue::String(value) => Some(SlashValue::String(value.clone())),
        _ => None,
    }
}

fn invalid(command: &str, option: &str) -> InteractionRouteError {
    InteractionRouteError::InvalidOption {
        command: command.into(),
        option: option.into(),
    }
}

fn duplicate(command: &str, option: &str) -> InteractionRouteError {
    InteractionRouteError::DuplicateOption {
        command: command.into(),
        option: option.into(),
    }
}

fn deferred_channel_response() -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::DeferredChannelMessageWithSource,
        data: None,
    }
}
