use cdr_discord::commands::slash_commands;
use twilight_model::application::command::{CommandOptionType, CommandType};

type ExpectedOption = (&'static str, CommandOptionType, bool, bool);

const NONE: &[ExpectedOption] = &[];
const LIMIT: &[ExpectedOption] = &[("limit", CommandOptionType::Integer, false, false)];
const REQUIRED_REF: &[ExpectedOption] = &[("ref", CommandOptionType::String, true, false)];
const OPTIONAL_REF: &[ExpectedOption] = &[("ref", CommandOptionType::String, false, false)];
const SETTINGS: &[ExpectedOption] = &[
    ("ref", CommandOptionType::String, false, false),
    ("model", CommandOptionType::String, false, true),
    ("effort", CommandOptionType::String, false, true),
    ("speed", CommandOptionType::String, false, false),
];
const CONTEXT: &[ExpectedOption] = &[
    ("all_threads", CommandOptionType::Boolean, false, false),
    ("refresh", CommandOptionType::Boolean, false, false),
    ("limit", CommandOptionType::Integer, false, false),
];
const DAYS: &[ExpectedOption] = &[("days", CommandOptionType::Integer, false, false)];
const PROMPT: &[ExpectedOption] = &[("prompt", CommandOptionType::String, true, false)];

const EXPECTED: &[(&str, &str, &[ExpectedOption])] = &[
    ("help", "Show Discord Codex commands.", NONE),
    ("stop", "Stop the current Codex or AGY reply.", NONE),
    ("list", "Show recent Codex threads.", LIMIT),
    ("archived_list", "Show archived Codex threads.", LIMIT),
    ("use", "Select the active Codex thread.", REQUIRED_REF),
    ("status", "Show selected Codex thread status.", OPTIONAL_REF),
    (
        "settings",
        "Update Codex thread model, effort, or speed.",
        SETTINGS,
    ),
    (
        "where",
        "Show the Codex thread mapped to this Discord channel.",
        NONE,
    ),
    (
        "context",
        "Show context usage for this Codex thread.",
        CONTEXT,
    ),
    ("usage", "Show live Codex usage and rate limits.", DAYS),
    (
        "new",
        "Create a new Codex thread with the first prompt.",
        PROMPT,
    ),
    (
        "ask",
        "Send a prompt to the mapped or selected Codex thread.",
        PROMPT,
    ),
    (
        "interview",
        "Clarify a request before implementation.",
        PROMPT,
    ),
    ("doctor", "Run Codex bridge diagnostics.", NONE),
    (
        "approval",
        "Show existing Codex approval and input requests.",
        NONE,
    ),
    ("runners", "Show Discord runner queues.", NONE),
    (
        "retract",
        "Remove your latest queued ask for this Codex thread.",
        OPTIONAL_REF,
    ),
    ("mirror_check", "Check Discord mirror mappings.", NONE),
    (
        "bridge_sync",
        "Refresh Codex bridge state and Discord mirror.",
        LIMIT,
    ),
    ("qa_buttons", "Run Discord button QA smoke.", NONE),
];

#[test]
fn twilight_slash_models_match_preserved_and_restored_command_contracts() {
    let commands = slash_commands(true);
    assert_eq!(commands.len(), EXPECTED.len());
    for (command, (name, description, expected_options)) in commands.iter().zip(EXPECTED) {
        assert_eq!(command.kind, CommandType::ChatInput);
        assert_eq!(command.name, *name);
        assert_eq!(command.description, *description);
        assert_eq!(command.options.len(), expected_options.len());
        for (option, (option_name, kind, required, autocomplete)) in
            command.options.iter().zip(*expected_options)
        {
            assert_eq!(option.name, *option_name);
            assert_eq!(option.kind, *kind);
            assert_eq!(option.description, "…");
            assert_eq!(option.required, required.then_some(true));
            assert_eq!(option.autocomplete, autocomplete.then_some(true));
            assert!(option.choices.is_none());
            assert!(option.min_value.is_none() && option.max_value.is_none());
        }
        assert_eq!(
            serde_json::to_value(command).expect("serializable command")["type"],
            1
        );
    }
}
