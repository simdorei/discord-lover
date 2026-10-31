use cdr_runtime::{
    action_executor::ActionExecutor, app_backend::AppServerTurnBackend, bridge_state::BridgeState,
    command_plan::CommandAction, queue_runner::QueueCoordinator,
};
use std::sync::Arc;
#[path = "support/archive_app_server.rs"]
mod support;

async fn scenario(name: &str, should_succeed: bool, archive_calls: usize) {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state.sqlite");
    rusqlite::Connection::open(&state)
        .unwrap()
        .execute_batch(include_str!("fixtures/action_state.sql"))
        .unwrap();
    let server = Arc::new(support::start(&temp, &state, name).await);
    let db = temp.path().join("mirror.sqlite");
    let bridge = Arc::new(BridgeState::new(temp.path().join("bridge.json")));
    bridge.set_selected_thread_id(Some("thread-b")).unwrap();
    let executor = ActionExecutor::new(
        state.clone(),
        db.clone(),
        bridge.clone(),
        Arc::new(QueueCoordinator::new(
            db.clone(),
            Arc::new(AppServerTurnBackend::new(server.clone())),
        )),
    )
    .with_server(server.clone());
    if name.starts_with("descendant_") {
        rusqlite::Connection::open(&state).unwrap().execute("INSERT INTO threads SELECT 'child','child',cwd,updated_at,rollout_path,model,reasoning_effort,tokens_used,0,0,source,thread_source FROM threads WHERE id='thread-b'",[]).unwrap();
    }
    if name == "queued" || name == "descendant_queued" {
        cdr_store::queue::enqueue(
            &db,
            cdr_store::queue::NewQueueJob {
                job_id: "pending",
                target_thread_id: if name == "descendant_queued" {
                    "child"
                } else {
                    "thread-b"
                },
                channel_id: 99,
                owner_user_id: Some(20),
                discord_message_id: Some(30),
                app_server_generation: 1,
                prompt: "saved",
                queued: true,
                ack_sent: true,
                created_at: 1.0,
            },
        )
        .unwrap();
    }
    if name == "intake" {
        cdr_store::prompt_intake::admit_prompt_intake(
            &db,
            cdr_store::prompt_intake::NewPromptIntake {
                job_id: "intake",
                target_thread_id: "thread-b",
                channel_id: 99,
                owner_user_id: Some(20),
                discord_message_id: Some(30),
                raw_prompt: "saved",
                auto_queue_when_busy: true,
                require_current_mirror: false,
                created_at: 1.0,
            },
        )
        .unwrap();
    }
    let result = executor
        .execute(CommandAction::Archive { reference: None }, 99, 20)
        .await;
    assert_eq!(result.is_ok(), should_succeed, "{name}: {result:?}");
    assert_eq!(
        support::calls(&temp)
            .iter()
            .filter(|v| v["method"] == "thread/archive")
            .count(),
        archive_calls
    );
    assert!(
        !support::calls(&temp)
            .iter()
            .any(|v| matches!(v["method"].as_str(), Some("thread/fork" | "turn/start")))
    );
    assert_eq!(
        bridge.selected_thread_id().unwrap().is_none(),
        should_succeed
    );
    if should_succeed {
        assert_archived(&state);
    }
    if name == "queued" || name == "descendant_queued" {
        assert_eq!(cdr_store::queue::list(&db).unwrap().len(), 1);
    }
    if name == "intake" {
        assert_eq!(
            cdr_store::prompt_intake::list_prompt_intakes(&db)
                .unwrap()
                .len(),
            1
        );
    }
    server.close().await.unwrap();
}

fn assert_archived(state: &std::path::Path) {
    assert!(
        cdr_codex_state::CodexThreadStore::open(state)
            .unwrap()
            .load_thread("thread-b", true)
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn archive_preserves_queued_and_intake_work_while_archiving() {
    scenario("queued", true, 1).await;
    scenario("intake", true, 1).await;
}

#[tokio::test]
async fn archive_does_not_resume_or_require_an_idle_turn() {
    scenario("became_active", true, 1).await;
}

#[tokio::test]
async fn archive_requires_persisted_effect_before_reporting_success() {
    scenario("no_persistence", false, 1).await;
    scenario("normal", true, 1).await;
}

#[tokio::test]
async fn descendant_work_is_preserved_and_invalid_inventory_is_rejected() {
    scenario("descendant_queued", true, 1).await;
    scenario("scope_malformed", false, 0).await;
}

#[tokio::test]
async fn descendant_archive_effects_are_verified_together() {
    scenario("descendant_partial", false, 1).await;
    scenario("descendant_normal", true, 1).await;
}

#[tokio::test]
async fn changed_scope_is_rejected_and_native_descendant_effects_are_verified() {
    scenario("descendant_changed", false, 0).await;
    scenario("descendant_writer", false, 1).await;
}

#[tokio::test]
async fn invalid_scope_pagination_never_dispatches_archive() {
    for name in ["scope_cursor_repeat", "scope_missing_cursor", "scope_root"] {
        scenario(name, false, 0).await;
    }
}

#[tokio::test]
async fn archive_does_not_depend_on_resumed_or_idle_status() {
    for name in [
        "active_without_event",
        "missing_status",
        "wrong_read_identity",
        "conflicting_read",
        "missing_nested_read",
        "conflicting_resume",
        "missing_nested_resume",
    ] {
        scenario(name, true, 1).await;
    }
}
