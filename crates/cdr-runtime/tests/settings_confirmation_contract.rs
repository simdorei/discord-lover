use cdr_runtime::{
    action_executor::ActionExecutor, app_backend::AppServerTurnBackend, bridge_state::BridgeState,
    command_plan::CommandAction, queue_runner::QueueCoordinator,
};
use std::sync::Arc;
#[path = "support/settings_app_server.rs"]
mod app;

#[tokio::test]
async fn settings_rejection_or_invalid_original_resume_never_claims_success() {
    for (mode, expected, updates) in [
        ("reject", "fixture settings rejection", 1),
        ("wrong-thread", "different or missing thread", 0),
        ("incomplete-resume", "setting field missing", 0),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("rpc.jsonl");
        let server = Arc::new(app::start(&temp, &log, mode).await);
        let (executor, bridge) = setup(&temp, server.clone());
        let before = std::fs::read(bridge.path()).unwrap();
        let result = executor
            .execute(
                CommandAction::Settings {
                    reference: Some("thread-b".into()),
                    model: Some("model-b".into()),
                    effort: Some("medium".into()),
                    speed: Some("fast".into()),
                },
                42,
                3,
            )
            .await;
        assert!(result.unwrap_err().to_string().contains(expected), "{mode}");
        assert_eq!(std::fs::read(bridge.path()).unwrap(), before);
        server.close().await.unwrap();
        let frames = frames(&log);
        assert_eq!(
            frames
                .iter()
                .filter(|v| v["method"] == "thread/settings/update")
                .count(),
            updates
        );
        assert!(!frames.iter().any(|v| matches!(
            v["method"].as_str(),
            Some("thread/fork" | "turn/start" | "thread/start")
        )));
    }
}

#[tokio::test]
async fn exact_reference_multi_change_and_standard_clear_are_verified() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("rpc.jsonl");
    let server = Arc::new(app::start(&temp, &log, "normal").await);
    let (executor, bridge) = setup(&temp, server.clone());
    for speed in ["fast", "standard"] {
        let result = executor
            .execute(
                CommandAction::Settings {
                    reference: Some("thread-b".into()),
                    model: Some("Model B".into()),
                    effort: Some("medium".into()),
                    speed: Some(speed.into()),
                },
                42,
                3,
            )
            .await
            .unwrap();
        assert!(
            result.text.contains("다음 요청부터 적용")
                && result.text.contains("model-b")
                && result.text.contains("medium")
                && result.text.contains(speed)
        );
        let stored = bridge.thread_settings("thread-b").unwrap();
        assert_eq!(stored.model.as_deref(), Some("model-b"));
        assert_eq!(stored.speed.as_deref(), Some(speed));
    }
    assert_eq!(
        bridge.selected_thread_id().unwrap().as_deref(),
        Some("thread-a")
    );
    server.close().await.unwrap();
    let frames = frames(&log);
    let updates: Vec<_> = frames
        .iter()
        .filter(|v| v["method"] == "thread/settings/update")
        .collect();
    assert_eq!(updates.len(), 2);
    assert!(
        updates
            .iter()
            .all(|v| v["params"]["threadId"] == "thread-b")
    );
    assert_eq!(updates[0]["params"]["serviceTier"], "priority");
    assert!(updates[1]["params"].get("serviceTier").unwrap().is_null());
}

#[tokio::test]
async fn standard_accepts_default_tier_and_already_applied_settings_without_notifications() {
    for (mode, update_count) in [("standard-clear", 1), ("standard-noop", 0), ("missing", 0)] {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("rpc.jsonl");
        let server = Arc::new(app::start(&temp, &log, mode).await);
        let (executor, bridge) = setup(&temp, server.clone());
        let result = executor
            .execute(
                CommandAction::Settings {
                    reference: Some("thread-b".into()),
                    model: None,
                    effort: None,
                    speed: Some("standard".into()),
                },
                42,
                3,
            )
            .await
            .unwrap();
        assert!(
            result.text.contains("속도: standard"),
            "{mode}: {}",
            result.text
        );
        assert!(!result.text.contains("속도: default"));
        if update_count == 0 {
            assert!(result.text.contains("이미 적용된"));
        }
        assert_eq!(
            bridge.thread_settings("thread-b").unwrap().speed.as_deref(),
            Some("standard")
        );
        assert_eq!(
            bridge.selected_thread_id().unwrap().as_deref(),
            Some("thread-a")
        );
        server.close().await.unwrap();
        let calls = frames(&log);
        assert_eq!(
            calls
                .iter()
                .filter(|v| v["method"] == "thread/resume")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|v| v["method"] == "thread/settings/update")
                .count(),
            update_count
        );
        assert!(!calls.iter().any(|v| matches!(
            v["method"].as_str(),
            Some("thread/fork" | "turn/start" | "thread/start")
        )));
    }
}

fn setup(
    temp: &tempfile::TempDir,
    server: Arc<cdr_app_server::ResidentAppServer>,
) -> (ActionExecutor<AppServerTurnBackend>, Arc<BridgeState>) {
    let state = temp.path().join("state.sqlite");
    rusqlite::Connection::open(&state)
        .unwrap()
        .execute_batch(include_str!("fixtures/action_state.sql"))
        .unwrap();
    let db = temp.path().join("mirror.sqlite");
    cdr_store::mapping::upsert_thread(&db, "thread-a", "project", "title", 100, 42, 1.0).unwrap();
    let bridge = Arc::new(BridgeState::new(temp.path().join("bridge.json")));
    // A different room/default target must not override the explicit reference.
    bridge.set_selected_thread_id(Some("thread-a")).unwrap();
    (
        ActionExecutor::new(
            state,
            db.clone(),
            bridge.clone(),
            Arc::new(QueueCoordinator::new(
                db,
                Arc::new(AppServerTurnBackend::new(server.clone())),
            )),
        )
        .with_server(server),
        bridge,
    )
}

fn frames(log: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
