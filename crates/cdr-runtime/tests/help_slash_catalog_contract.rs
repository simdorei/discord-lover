use cdr_discord::{
    commands::slash_commands,
    interaction::{RoutedWork, route_command},
};
use cdr_runtime::command_plan::{CommandAction, CommandPlanError, plan_slash};
use std::collections::BTreeSet;
use twilight_model::{
    application::{
        command::{CommandOptionType, CommandType},
        interaction::{
            InteractionType,
            application_command::{CommandData, CommandDataOption, CommandOptionValue},
        },
    },
    id::Id,
};

const HELP: &str = include_str!("../src/action_executor/help.txt");

#[test]
fn documented_slash_names_and_each_registered_option_reach_their_actual_actions() {
    let line = HELP
        .lines()
        .find(|line| line.starts_with("등록된 항목: "))
        .unwrap();
    let mut documented = line
        .trim_start_matches("등록된 항목: ")
        .split_whitespace()
        .map(|name| name.trim_start_matches('/'))
        .collect::<BTreeSet<_>>();
    documented.extend(["ask", "use"]);
    let registered = slash_commands(false);
    assert_eq!(
        documented,
        registered
            .iter()
            .map(|command| command.name.as_str())
            .collect()
    );
    for command in registered {
        let options = command
            .options
            .iter()
            // Automatic policy is a separate action, covered below for both values.
            .filter(|option| command.name != "settings" || option.name != "auto_reserve")
            .map(|option| CommandDataOption {
                name: option.name.clone(),
                value: match option.kind {
                    CommandOptionType::Integer => CommandOptionValue::Integer(3),
                    CommandOptionType::Boolean => CommandOptionValue::Boolean(true),
                    CommandOptionType::String => CommandOptionValue::String(
                        match option.name.as_str() {
                            "ref" => "thread-b",
                            "model" => "fixture-model",
                            "effort" => "high",
                            "speed" => "fast",
                            "prompt" => "실제 요청",
                            other => panic!("undocumented string option: {other}"),
                        }
                        .into(),
                    ),
                    other => panic!("undocumented option type: {other:?}"),
                },
            })
            .collect();
        let data = CommandData {
            guild_id: None,
            id: Id::new(1),
            name: command.name.clone(),
            kind: CommandType::ChatInput,
            options,
            resolved: None,
            target_id: None,
        };
        let route = route_command(&data, InteractionType::ApplicationCommand, false).unwrap();
        let Some(RoutedWork::Slash(invocation)) = route.work else {
            panic!("not slash")
        };
        let action = plan_slash(&invocation).unwrap();
        let reference = Some("thread-b".into());
        let expected = match command.name.as_str() {
            "help" => CommandAction::Help,
            "list" => CommandAction::List { limit: 3 },
            "archived_list" => CommandAction::ArchivedList { limit: 3 },
            "use" => CommandAction::Use {
                reference: "thread-b".into(),
            },
            "status" => CommandAction::Status { reference },
            "settings" => CommandAction::Settings {
                reference,
                model: Some("fixture-model".into()),
                effort: Some("high".into()),
                speed: Some("fast".into()),
            },
            "where" => CommandAction::Where,
            "context" => CommandAction::Context {
                all_threads: true,
                refresh: true,
                limit: 3,
            },
            "usage" => CommandAction::Usage { days: 3 },
            "new" => CommandAction::New {
                prompt: "실제 요청".into(),
            },
            "ask" => CommandAction::Ask {
                prompt: "실제 요청".into(),
            },
            "interview" => CommandAction::Interview {
                prompt: "실제 요청".into(),
            },
            "doctor" => CommandAction::Doctor,
            "approval" => CommandAction::Approval,
            "runners" => CommandAction::Runners,
            "stop" => CommandAction::Stop { reference: None },
            "retract" => CommandAction::Retract { reference },
            "mirror_check" => CommandAction::MirrorCheck,
            "bridge_sync" => CommandAction::BridgeSync { limit: Some(3) },
            other => panic!("undocumented command: {other}"),
        };
        assert_eq!(action, expected, "{}", command.name);
    }
}

#[test]
fn settings_schema_advertises_only_manual_options() {
    let settings = slash_commands(false)
        .into_iter()
        .find(|c| c.name == "settings")
        .unwrap();
    assert_eq!(
        settings
            .options
            .iter()
            .map(|o| o.name.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["ref", "model", "effort", "speed"])
    );
}

#[test]
fn automatic_policy_rejects_every_nonempty_manual_option_combination() {
    for enabled in [false, true] {
        for reference in [None, Some("thread-b")] {
            for mask in 0..8 {
                let error = route_settings(settings_options(enabled, reference, mask)).unwrap_err();
                assert!(
                    matches!(
                        error,
                        CommandPlanError::Unsupported(ref message)
                            if message == cdr_runtime::command_plan::AUTO_RESERVE_REMOVED
                    ),
                    "enabled={enabled} reference={reference:?} manual_mask={mask}: {error}"
                );
            }
        }
    }
}

fn settings_options(
    enabled: bool,
    reference: Option<&str>,
    manual_mask: u8,
) -> Vec<CommandDataOption> {
    let mut options = vec![CommandDataOption {
        name: "auto_reserve".into(),
        value: CommandOptionValue::Boolean(enabled),
    }];
    if let Some(reference) = reference {
        options.push(CommandDataOption {
            name: "ref".into(),
            value: CommandOptionValue::String(reference.into()),
        });
    }
    for (bit, name, value) in [
        (1, "model", "fixture-model"),
        (2, "effort", "high"),
        (4, "speed", "fast"),
    ] {
        if manual_mask & bit != 0 {
            options.push(CommandDataOption {
                name: name.into(),
                value: CommandOptionValue::String(value.into()),
            });
        }
    }
    options
}

fn route_settings(options: Vec<CommandDataOption>) -> Result<CommandAction, CommandPlanError> {
    let data = CommandData {
        guild_id: None,
        id: Id::new(1),
        name: "settings".into(),
        kind: CommandType::ChatInput,
        options,
        resolved: None,
        target_id: None,
    };
    let route = route_command(&data, InteractionType::ApplicationCommand, false)
        .map_err(|e| CommandPlanError::Unsupported(e.to_string()))?;
    let Some(RoutedWork::Slash(invocation)) = route.work else {
        panic!("settings did not route as slash")
    };
    plan_slash(&invocation)
}
