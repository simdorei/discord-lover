use cdr_discord::commands::slash_command_names;
use cdr_discord::components::{
    ApprovalAnswer, BusyAction, ComponentId, approval_button_row, busy_button_row, deferred_update,
    format_input_choice, input_button_row, parse_component_id, persistent_claim_key,
};
use cdr_discord::intake::{
    GateReason, MessageContentAccess, MessageFacts, Permission, RuntimeState, gate_message,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct Inventory {
    commands: Vec<InventoryCommand>,
}

#[derive(Deserialize)]
struct InventoryCommand {
    name: String,
    conditional: bool,
}

#[test]
fn dc1_slash_inventory_preserves_supported_python_commands_but_excludes_ipc() {
    let inventory: Inventory = serde_json::from_str(include_str!(
        "../../../fixtures/parity/discord_commands.json"
    ))
    .expect("fixture");
    let normal = slash_command_names(false);
    let qa = slash_command_names(true);
    let expected = inventory
        .commands
        .iter()
        // The historical Python fixture retains ask_ipc. The user explicitly
        // removed that transport from the Rust runtime, including QA mode.
        .filter(|command| !command.conditional && command.name != "ask_ipc")
        .map(|command| command.name.as_str())
        .collect::<Vec<_>>();
    // R09 exposes the existing prefix approval redisplay via slash too.
    assert!(normal.contains(&"approval"));
    assert!(normal.contains(&"stop"));
    assert_eq!(
        normal
            .iter()
            .copied()
            .filter(|name| !matches!(*name, "approval" | "stop"))
            .collect::<Vec<_>>(),
        expected
    );
    let expected_qa = inventory
        .commands
        .iter()
        .filter(|command| command.name != "ask_ipc")
        .map(|command| command.name.as_str())
        .collect::<Vec<_>>();
    assert!(qa.contains(&"approval"));
    assert!(qa.contains(&"stop"));
    assert_eq!(
        qa.iter()
            .copied()
            .filter(|name| !matches!(*name, "approval" | "stop"))
            .collect::<Vec<_>>(),
        expected_qa
    );
    assert!(!normal.contains(&"ask_ipc"));
    assert!(!qa.contains(&"ask_ipc"));
    assert!(!normal.contains(&"qa_buttons"));
    assert!(qa.contains(&"qa_buttons"));
}

#[test]
fn dc2_component_ids_are_strict_bounded_and_claimable_once_by_message_key() {
    let choice = "0123456789abcdef01234567";
    assert_eq!(
        parse_component_id(&format!("codex_busy:{choice}:steer")),
        Some(ComponentId::Busy {
            choice_id: choice.into(),
            action: BusyAction::Steer,
        })
    );
    assert_eq!(
        parse_component_id("codex_approval:thread-1:2"),
        Some(ComponentId::Approval {
            thread_id: "thread-1".into(),
            answer: ApprovalAnswer::ApproveSession,
        })
    );
    assert!(parse_component_id("codex_busy:not-hex:stop").is_none());
    let input = format_input_choice(&"t".repeat(70), "safe_value").expect("bounded input");
    assert!(input.chars().count() <= 100);
    assert!(format_input_choice(&"t".repeat(90), "safe_value").is_err());
    assert!(format_input_choice("thread", "unsafe value").is_err());
    assert_eq!(
        persistent_claim_key(42, "codex_approval:thread-1:1"),
        persistent_claim_key(42, "codex_approval:different:3")
    );
    assert_ne!(
        persistent_claim_key(42, "codex_approval:thread-1:1"),
        persistent_claim_key(43, "codex_approval:thread-1:1")
    );
}

#[test]
fn dc3_twilight_buttons_and_deferred_ack_keep_the_existing_wire_shape() {
    let row = busy_button_row("0123456789abcdef01234567", false).expect("row");
    let row_json = serde_json::to_value(row).expect("row json");
    assert_eq!(row_json["type"], 1);
    assert_eq!(row_json["components"].as_array().expect("buttons").len(), 4);
    assert_eq!(
        row_json["components"][0]["custom_id"],
        "codex_busy:0123456789abcdef01234567:steer"
    );
    // Enabled is represented by omission on Twilight's Discord wire payload.
    assert_eq!(
        row_json["components"][0]["disabled"],
        serde_json::Value::Null
    );
    assert_eq!(
        serde_json::to_value(deferred_update()).expect("ack json"),
        serde_json::json!({ "type": 6 })
    );
}

#[test]
fn dc4_message_intake_order_matches_python_fail_closed_gate() {
    let disabled = gate_message(MessageFacts {
        message_content: MessageContentAccess::Disabled,
        channel: Permission::Allowed,
        user: Permission::Allowed,
        bot_bridge_mention: true,
        runtime: RuntimeState::Running,
    });
    assert_eq!(disabled.reason, GateReason::MessageContentDisabled);
    assert!(!disabled.bot_bridge_mention);

    let channel = gate_message(MessageFacts {
        message_content: MessageContentAccess::Enabled,
        channel: Permission::Denied,
        user: Permission::Allowed,
        bot_bridge_mention: true,
        runtime: RuntimeState::Running,
    });
    assert_eq!(channel.reason, GateReason::ChannelNotAllowed);
    assert!(!channel.bot_bridge_mention);

    let stopping = gate_message(MessageFacts {
        message_content: MessageContentAccess::Enabled,
        channel: Permission::Allowed,
        user: Permission::Allowed,
        bot_bridge_mention: true,
        runtime: RuntimeState::Stopping,
    });
    assert_eq!(stopping.reason, GateReason::Stopping);
    assert!(stopping.handled && stopping.restart_notice && stopping.bot_bridge_mention);
}

#[test]
fn dc5_approval_and_input_rows_keep_labels_and_safe_persistent_ids() {
    let approval = serde_json::to_value(approval_button_row("thread-1").unwrap()).unwrap();
    assert_eq!(approval["components"].as_array().unwrap().len(), 4);
    assert_eq!(approval["components"][0]["label"], "Approve");
    assert_eq!(
        approval["components"][1]["custom_id"],
        "codex_approval:thread-1:2"
    );

    let options = vec![("1".to_owned(), "Recommended choice".to_owned())];
    let input = serde_json::to_value(input_button_row("thread-1", &options).unwrap()).unwrap();
    assert_eq!(input["components"][0]["label"], "Recommended choice");
    assert_eq!(
        input["components"][0]["custom_id"],
        "codex_input:thread-1:1"
    );
}
