use cdr_runtime::{
    action_executor::{ActionContext, ActionExecutor},
    app_backend::AppServerTurnBackend,
    bridge_state::BridgeState,
    command_plan::CommandAction,
    message_plan::MessagePlan,
    queue_runner::QueueCoordinator,
};
use cdr_store::ingress::{self, IngressKind, NewIngress};
use serde_json::json;
use std::{path::Path, sync::Arc};
#[path = "support/archive_app_server.rs"]
mod support;

fn stage(db: &Path, id: i64, target: Option<&str>, own: bool, content: &str) {
    let action = if own {
        CommandAction::Archive { reference: None }
    } else {
        CommandAction::Ask {
            prompt: content.into(),
        }
    };
    ingress::admit(db, &NewIngress {
        ingress_id: format!("message:{id}"), kind: IngressKind::Message, event_id: Some(id),
        application_id: None, channel_id: 99, owner_user_id: 20, source_message_id: Some(id),
        payload: json!({"version":1,"content":content,"plan":MessagePlan::Execute(action),"processing_mode":"normal"}),
        target_thread_id: target.map(str::to_owned), canonical_owner: None, now: 1.0,
    }).unwrap();
    if own {
        assert!(
            ingress::begin_execution(db, &format!("message:{id}"), "processing", target, 2.0)
                .unwrap()
        );
    }
}

async fn scenario(name: &str, expected: bool) {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state.sqlite");
    rusqlite::Connection::open(&state)
        .unwrap()
        .execute_batch(include_str!("fixtures/action_state.sql"))
        .unwrap();
    let server = Arc::new(support::start(&temp, &state, "normal").await);
    let db = temp.path().join("mirror.sqlite");
    let bridge = Arc::new(BridgeState::new(temp.path().join("bridge.json")));
    bridge.set_selected_thread_id(Some("thread-b")).unwrap();
    let executor = ActionExecutor::new(
        state,
        db.clone(),
        bridge.clone(),
        Arc::new(QueueCoordinator::new(
            db.clone(),
            Arc::new(AppServerTurnBackend::new(server.clone())),
        )),
    )
    .with_server(server.clone());
    stage(
        &db,
        1,
        Some("thread-b"),
        true,
        if name == "borrowed_command" {
            "!help"
        } else {
            "!archive"
        },
    );
    match name {
        "pending" => stage(&db, 2, Some("thread-b"), false, "next request"),
        "unbound" => stage(&db, 2, None, false, "not selected yet"),
        "independent" => stage(&db, 2, Some("thread-a"), false, "other thread"),
        "completed" => {
            stage(&db, 2, Some("thread-b"), false, "finished request");
            assert!(ingress::begin_execution(&db, "message:2", "processing", None, 2.0).unwrap());
            ingress::record_result(&db, "message:2", &json!({"action_completed":true}), 3.0)
                .unwrap();
        }
        _ => {}
    }
    let preserved = ingress::get(&db, "message:2").unwrap();
    let result = executor
        .execute_with_context(
            CommandAction::Archive { reference: None },
            ActionContext {
                channel_id: 99,
                user_id: if name == "borrowed_user" { 21 } else { 20 },
                discord_message_id: Some(1),
                auto_queue_when_busy: false,
            },
        )
        .await;
    server.close().await.unwrap();
    assert_eq!(result.is_ok(), expected, "{name}: {result:?}");
    let calls = support::calls(&temp);
    assert_eq!(
        calls
            .iter()
            .filter(|v| v["method"] == "thread/archive")
            .count(),
        usize::from(expected),
        "{name}"
    );
    assert_eq!(bridge.selected_thread_id().unwrap().is_none(), expected);
    assert_eq!(
        ingress::get(&db, "message:2").unwrap(),
        preserved,
        "other requests must remain intact"
    );
}

#[tokio::test]
async fn pending_and_unbound_ingress_does_not_block_explicit_archive() {
    scenario("pending", true).await;
    scenario("unbound", true).await;
}

#[tokio::test]
async fn archive_excludes_only_its_exact_active_command_envelope() {
    scenario("own", true).await;
    scenario("borrowed_command", false).await;
    scenario("borrowed_user", false).await;
}

#[tokio::test]
async fn finished_or_proven_other_target_does_not_block_archive() {
    scenario("completed", true).await;
    scenario("independent", true).await;
}
