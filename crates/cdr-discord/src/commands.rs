use twilight_model::{
    application::command::{Command, CommandOption, CommandOptionType, CommandType},
    id::Id,
};

#[derive(Clone, Copy)]
struct OptionSpec {
    name: &'static str,
    kind: CommandOptionType,
    required: bool,
    autocomplete: bool,
}

#[derive(Clone, Copy)]
struct CommandSpec {
    name: &'static str,
    description: &'static str,
    options: &'static [OptionSpec],
    conditional: bool,
}

const fn option(
    name: &'static str,
    kind: CommandOptionType,
    required: bool,
    autocomplete: bool,
) -> OptionSpec {
    OptionSpec {
        name,
        kind,
        required,
        autocomplete,
    }
}

const LIMIT: &[OptionSpec] = &[option("limit", CommandOptionType::Integer, false, false)];
const REF_REQUIRED: &[OptionSpec] = &[option("ref", CommandOptionType::String, true, false)];
const REF_OPTIONAL: &[OptionSpec] = &[option("ref", CommandOptionType::String, false, false)];
const SETTINGS: &[OptionSpec] = &[
    option("ref", CommandOptionType::String, false, false),
    option("model", CommandOptionType::String, false, true),
    option("effort", CommandOptionType::String, false, true),
    option("speed", CommandOptionType::String, false, false),
];
const CONTEXT: &[OptionSpec] = &[
    option("all_threads", CommandOptionType::Boolean, false, false),
    option("refresh", CommandOptionType::Boolean, false, false),
    option("limit", CommandOptionType::Integer, false, false),
];
const DAYS: &[OptionSpec] = &[option("days", CommandOptionType::Integer, false, false)];
const PROMPT: &[OptionSpec] = &[option("prompt", CommandOptionType::String, true, false)];

const SLASH_COMMANDS: &[CommandSpec] = &[
    command("help", "Show Discord Codex commands.", &[], false),
    command("stop", "Stop the current Codex or AGY reply.", &[], false),
    command("list", "Show recent Codex threads.", LIMIT, false),
    command(
        "archived_list",
        "Show archived Codex threads.",
        LIMIT,
        false,
    ),
    command(
        "use",
        "Select the active Codex thread.",
        REF_REQUIRED,
        false,
    ),
    command(
        "status",
        "Show selected Codex thread status.",
        REF_OPTIONAL,
        false,
    ),
    command(
        "settings",
        "Update Codex thread model, effort, or speed.",
        SETTINGS,
        false,
    ),
    command(
        "where",
        "Show the Codex thread mapped to this Discord channel.",
        &[],
        false,
    ),
    command(
        "context",
        "Show context usage for this Codex thread.",
        CONTEXT,
        false,
    ),
    command(
        "usage",
        "Show live Codex usage and rate limits.",
        DAYS,
        false,
    ),
    command(
        "new",
        "Create a new Codex thread with the first prompt.",
        PROMPT,
        false,
    ),
    command(
        "ask",
        "Send a prompt to the mapped or selected Codex thread.",
        PROMPT,
        false,
    ),
    command(
        "interview",
        "Clarify a request before implementation.",
        PROMPT,
        false,
    ),
    command("doctor", "Run Codex bridge diagnostics.", &[], false),
    command(
        "approval",
        "Show existing Codex approval and input requests.",
        &[],
        false,
    ),
    command("runners", "Show Discord runner queues.", &[], false),
    command(
        "retract",
        "Remove your latest queued ask for this Codex thread.",
        REF_OPTIONAL,
        false,
    ),
    command("mirror_check", "Check Discord mirror mappings.", &[], false),
    command(
        "bridge_sync",
        "Refresh Codex bridge state and Discord mirror.",
        LIMIT,
        false,
    ),
    command("qa_buttons", "Run Discord button QA smoke.", &[], true),
];

const fn command(
    name: &'static str,
    description: &'static str,
    options: &'static [OptionSpec],
    conditional: bool,
) -> CommandSpec {
    CommandSpec {
        name,
        description,
        options,
        conditional,
    }
}

#[must_use]
pub fn slash_command_names(qa_enabled: bool) -> Vec<&'static str> {
    selected_specs(qa_enabled).map(|spec| spec.name).collect()
}

#[must_use]
pub fn slash_commands(qa_enabled: bool) -> Vec<Command> {
    selected_specs(qa_enabled).map(build_command).collect()
}

fn selected_specs(qa_enabled: bool) -> impl Iterator<Item = &'static CommandSpec> {
    SLASH_COMMANDS
        .iter()
        .filter(move |spec| qa_enabled || !spec.conditional)
}

#[allow(deprecated)]
fn build_command(spec: &CommandSpec) -> Command {
    Command {
        application_id: None,
        contexts: None,
        default_member_permissions: None,
        dm_permission: None,
        description: spec.description.to_owned(),
        description_localizations: None,
        guild_id: None,
        id: None,
        integration_types: None,
        kind: CommandType::ChatInput,
        name: spec.name.to_owned(),
        name_localizations: None,
        nsfw: None,
        options: spec.options.iter().map(build_option).collect(),
        version: Id::new(1),
    }
}

fn build_option(spec: &OptionSpec) -> CommandOption {
    CommandOption {
        autocomplete: spec.autocomplete.then_some(true),
        channel_types: None,
        choices: None,
        description: "…".to_owned(),
        description_localizations: None,
        kind: spec.kind,
        max_length: None,
        max_value: None,
        min_length: None,
        min_value: None,
        name: spec.name.to_owned(),
        name_localizations: None,
        options: None,
        required: spec.required.then_some(true),
    }
}
