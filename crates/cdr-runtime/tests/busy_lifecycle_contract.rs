use cdr_app_server::requests::AppRequest;
use cdr_runtime::{
    action_executor::{ActionExecutor, ActionUi},
    app_backend::AppServerTurnBackend,
    bridge_state::BridgeState,
    command_plan::CommandAction,
    queue_runner::QueueCoordinator,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
#[path = "support/action_app_server.rs"]
mod support;

#[tokio::test]
async fn timed_out_pending_request_offers_queue_and_ignore_without_unbound_controls() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("rpc.jsonl");
    let server = Arc::new(support::start_fake_server(&temp, &log).await);
    let db = temp.path().join("mirror.sqlite");
    cdr_store::queue::enqueue(
        &db,
        cdr_store::queue::NewQueueJob {
            job_id: "timed-out",
            target_thread_id: "thread-a",
            channel_id: 10,
            owner_user_id: Some(20),
            discord_message_id: Some(100),
            app_server_generation: 1,
            prompt: "??",
            queued: false,
            ack_sent: true,
            created_at: 1.0,
        },
    )
    .unwrap();
    let bridge = Arc::new(BridgeState::new(temp.path().join("bridge.json")));
    bridge.set_selected_thread_id(Some("thread-a")).unwrap();
    let executor = ActionExecutor::new(
        temp.path().join("state.sqlite"),
        db.clone(),
        bridge,
        Arc::new(QueueCoordinator::new(
            db.clone(),
            Arc::new(AppServerTurnBackend::new(server.clone())),
        )),
    )
    .with_server(server.clone());
    let result = executor
        .execute(
            CommandAction::Ask {
                prompt: "next request".into(),
            },
            10,
            20,
        )
        .await
        .unwrap();
    let Some(ActionUi::PendingBusy { ref choice_id }) = result.ui else {
        panic!("pending must have no active-turn controls");
    };
    let buttons =
        serde_json::to_value(cdr_runtime::action_ui::render_action_ui(result.ui.as_ref()).unwrap())
            .unwrap();
    let ids: Vec<_> = buttons[0]["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["custom_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            format!("codex_busy:{choice_id}:queue"),
            format!("codex_busy:{choice_id}:ignore")
        ]
    );
    assert_eq!(
        cdr_store::control_binding::resolve(&db, choice_id, "thread-a").unwrap(),
        None
    );
    let jobs = cdr_store::queue::list(&db).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].job_id, "timed-out");
    assert!(jobs[0].turn_id.is_none());
    server.close().await.unwrap();
    assert!(!support::rpc_log(&log).iter().any(|r| matches!(
        r["method"].as_str(),
        Some("turn/start" | "turn/steer" | "turn/interrupt")
    )));
}

#[tokio::test]
async fn steer_now_label_uses_the_same_lifecycle_readiness_as_click_validation() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("rpc.jsonl");
    let server = Arc::new(support::start_fake_server(&temp, &log).await);
    server
        .execute(
            AppRequest {
                method: "test/active-turn",
                params: json!({"threadId":"thread-a","turnId":"current"}),
                timeout: Duration::from_secs(2),
            },
            None,
        )
        .await
        .unwrap();
    let db = temp.path().join("mirror.sqlite");
    let bridge = Arc::new(BridgeState::new(temp.path().join("bridge.json")));
    bridge.set_selected_thread_id(Some("thread-a")).unwrap();
    let executor = ActionExecutor::new(
        temp.path().join("state.sqlite"),
        db.clone(),
        bridge,
        Arc::new(QueueCoordinator::new(
            db,
            Arc::new(AppServerTurnBackend::new(server.clone())),
        )),
    )
    .with_server(server.clone());
    for ready in [true, false] {
        if !ready {
            assert!(!server.force_restart_if_quiescent().await.unwrap());
        }
        let result = executor
            .execute(
                CommandAction::Ask {
                    prompt: "additional direction".into(),
                },
                10,
                20,
            )
            .await
            .unwrap();
        let ActionUi::Busy {
            allow_steer,
            choice_id,
        } = result.ui.unwrap()
        else {
            panic!("expected busy controls")
        };
        assert_eq!(
            allow_steer, ready,
            "Steer now must not be shown while the resident refuses controls"
        );
        let bound =
            cdr_store::control_binding::resolve(executor.mirror_db(), &choice_id, "thread-a")
                .unwrap();
        assert_eq!(
            bound.as_deref(),
            Some("current"),
            "original turn binding must remain immutable even while temporarily not ready"
        );
        assert_eq!(
            executor
                .verified_control_turn(10, "thread-a", bound.as_deref())
                .await
                .is_ok(),
            ready
        );
    }
    server.close().await.unwrap();
    assert!(!support::rpc_log(&log).iter().any(|v| matches!(
        v["method"].as_str(),
        Some("thread/fork" | "thread/resume" | "turn/start" | "turn/steer")
    )));
}
