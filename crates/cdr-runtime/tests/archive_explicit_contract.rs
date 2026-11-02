use cdr_app_server::{
    AppServerError, ResidentAppServer,
    idle_release::{IdleReleaseJournal, IdleReleaseToken},
};
use cdr_runtime::{
    action_executor::ActionExecutor, app_backend::AppServerTurnBackend, bridge_state::BridgeState,
    command_plan::CommandAction, dead_generation_recovery::RuntimeDeadGenerationFence,
    queue_runner::QueueCoordinator, soak::native_fixture,
};
use cdr_store::{
    ingress::{self, IngressKind, NewIngress},
    queue,
};
use serde_json::json;
use std::sync::Arc;

#[allow(dead_code)]
#[path = "support/archive_app_server.rs"]
mod app;

struct HeldIdleJournal;

impl HeldIdleJournal {
    fn error() -> AppServerError {
        AppServerError::IdleRelease {
            message: "old idle-release hold".into(),
        }
    }
}

impl IdleReleaseJournal for HeldIdleJournal {
    fn before_mutation(
        &self,
        _: &str,
        _: u64,
        _: &str,
    ) -> Result<Option<IdleReleaseToken>, AppServerError> {
        Err(Self::error())
    }
    fn check_mutation(&self, _: &str) -> Result<(), AppServerError> {
        Err(Self::error())
    }
    fn resume_required(&self, _: &str) -> Result<bool, AppServerError> {
        Ok(true)
    }
    fn verify(&self, _: &IdleReleaseToken, _: bool) -> Result<(), AppServerError> {
        Err(Self::error())
    }
    fn transition(
        &self,
        _: &IdleReleaseToken,
        _: &str,
        _: &str,
    ) -> Result<IdleReleaseToken, AppServerError> {
        Err(Self::error())
    }
    fn old_child_exited(&self, _: &str, _: u64) -> Result<(), AppServerError> {
        Ok(())
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep each scenario's native RPC and durable-state assertions with its fixture."
)]
async fn exercise(scenario: &str, saved_work: bool, prior_archive: bool) {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state.sqlite");
    rusqlite::Connection::open(&state)
        .unwrap()
        .execute_batch(include_str!("fixtures/action_state.sql"))
        .unwrap();
    let db = temp.path().join("mirror.sqlite");
    let fence = Arc::new(
        RuntimeDeadGenerationFence::new(db.clone(), "explicit-archive".into(), None).unwrap(),
    );
    let mut config = native_fixture::config("archive");
    config
        .environment
        .insert("ARCHIVE_STATE".into(), state.to_string_lossy().into_owned());
    config.environment.insert(
        "ARCHIVE_LOG".into(),
        temp.path().join("rpc.jsonl").to_string_lossy().into_owned(),
    );
    config
        .environment
        .insert("ARCHIVE_SCENARIO".into(), scenario.into());
    let server = Arc::new(
        ResidentAppServer::start_with_dead_generation_fence(config, fence)
            .await
            .unwrap(),
    );
    let bridge = Arc::new(BridgeState::new(temp.path().join("bridge.json")));
    bridge.set_selected_thread_id(Some("thread-b")).unwrap();
    let executor = ActionExecutor::new(
        state.clone(),
        db.clone(),
        bridge,
        Arc::new(QueueCoordinator::new(
            db.clone(),
            Arc::new(AppServerTurnBackend::new(server.clone())),
        )),
    )
    .with_server(server.clone());
    if scenario == "became_active" {
        server
            .execute(
                cdr_app_server::requests::AppRequest {
                    method: "thread/resume",
                    params: json!({"threadId":"thread-b"}),
                    timeout: std::time::Duration::from_secs(3),
                },
                None,
            )
            .await
            .unwrap();
        assert!(server.active_turn_id("thread-b").await.unwrap().is_some());
    }
    if saved_work {
        server
            .install_idle_release_journal(Arc::new(HeldIdleJournal))
            .unwrap();
        let resume = server
            .execute(
                cdr_app_server::requests::AppRequest {
                    method: "thread/resume",
                    params: json!({"threadId":"thread-b"}),
                    timeout: std::time::Duration::from_secs(3),
                },
                None,
            )
            .await;
        assert!(
            matches!(resume, Err(AppServerError::IdleRelease { .. })),
            "ordinary resume must remain held: {resume:?}"
        );
        ingress::admit(
            &db,
            &NewIngress {
                ingress_id: "message:7".into(),
                kind: IngressKind::Message,
                event_id: Some(7),
                application_id: None,
                channel_id: 99,
                owner_user_id: 20,
                source_message_id: Some(7),
                payload: json!({"version":1,"content":"!mirror sync"}),
                target_thread_id: Some("thread-b".into()),
                canonical_owner: None,
                now: 1.0,
            },
        )
        .unwrap();
        ingress::hold(&db, "message:7", "old incomplete request", true, 2.0).unwrap();
        queue::enqueue(
            &db,
            queue::NewQueueJob {
                job_id: "saved",
                target_thread_id: "thread-b",
                channel_id: 99,
                owner_user_id: Some(20),
                discord_message_id: Some(8),
                app_server_generation: 1,
                prompt: "saved input",
                queued: true,
                ack_sent: true,
                created_at: 1.0,
            },
        )
        .unwrap();
        cdr_store::prompt_intake::admit_prompt_intake(
            &db,
            cdr_store::prompt_intake::NewPromptIntake {
                job_id: "saved-intake",
                target_thread_id: "thread-b",
                channel_id: 99,
                owner_user_id: Some(20),
                discord_message_id: Some(9),
                raw_prompt: "saved intake",
                auto_queue_when_busy: true,
                require_current_mirror: false,
                created_at: 1.0,
            },
        )
        .unwrap();
        let sql = cdr_store::schema::open_initialized(&db).unwrap();
        sql.execute(
            "INSERT INTO cdr_execution_holds VALUES ('saved','thread-b','old stop','{}',1)",
            [],
        )
        .unwrap();
        sql.execute("INSERT INTO codex_delivery_receipts VALUES ('[99,\"message/reply/v1\",\"old\",0]','hash',NULL,1,NULL)", []).unwrap();
        sql.execute("INSERT INTO codex_mutation_attempts(attempt_id,runtime_id,owner_id,generation,wire_id,method,target_thread_id,scoped,request_sha256,state,created_at,updated_at) VALUES ('old','previous','previous',1,'old','thread/resume','thread-b',1,'hash','prepared',1,1)", []).unwrap();
    }
    if prior_archive {
        cdr_store::archive_fence::reserve(&db, &["thread-b".into()].into(), None).unwrap();
    }
    let jobs = queue::list(&db).unwrap();
    let original = ingress::get(&db, "message:7").unwrap();
    let prior_calls = app::calls(&temp).len();
    let intakes = cdr_store::prompt_intake::list_prompt_intakes(&db).unwrap();
    let result = executor
        .execute(CommandAction::Archive { reference: None }, 99, 20)
        .await;
    server.close().await.unwrap();
    let calls = app::calls(&temp);
    assert_eq!(
        calls
            .iter()
            .filter(|c| c["method"] == "thread/archive")
            .count(),
        1,
        "{result:?}"
    );
    assert!(!calls[prior_calls..].iter().any(|c| matches!(
        c["method"].as_str(),
        Some("thread/resume" | "thread/read" | "turn/start" | "thread/fork")
    )));
    assert_eq!(queue::list(&db).unwrap(), jobs);
    assert_eq!(
        cdr_store::prompt_intake::list_prompt_intakes(&db).unwrap(),
        intakes
    );
    assert_eq!(ingress::get(&db, "message:7").unwrap(), original);
    if scenario == "archive_writer_reject" {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("already has an active writer")
        );
        assert_eq!(
            cdr_store::archive_fence::target_is_fenced(&db, "thread-b").unwrap(),
            prior_archive
        );
    } else {
        result.unwrap();
        assert!(
            cdr_codex_state::CodexThreadStore::open(&state)
                .unwrap()
                .load_thread("thread-b", true)
                .unwrap()
                .is_some()
        );
        assert!(cdr_store::archive_fence::target_is_fenced(&db, "thread-b").unwrap());
        if saved_work {
            let sql = cdr_store::schema::open_initialized(&db).unwrap();
            assert_eq!(
                sql.query_row(
                    "SELECT state FROM codex_mutation_attempts WHERE attempt_id='old'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
                "prepared"
            );
            assert!(
                sql.execute(
                    "UPDATE codex_turn_queue SET state='starting' WHERE job_id='saved'",
                    []
                )
                .is_err()
            );
        }
    }
}

#[tokio::test]
async fn explicit_archive_overrides_old_processing_records_without_replaying_them() {
    Box::pin(exercise("normal", true, false)).await;
}

#[tokio::test]
async fn explicit_archive_preserves_native_writer_rejection_without_resuming() {
    Box::pin(exercise("archive_writer_reject", false, false)).await;
}

#[tokio::test]
async fn explicit_archive_does_not_require_the_owned_turn_to_be_idle() {
    Box::pin(exercise("became_active", false, false)).await;
}

#[tokio::test]
async fn explicit_retry_passes_prior_archive_fence_but_rejection_preserves_it() {
    for scenario in ["normal", "archive_writer_reject"] {
        Box::pin(exercise(scenario, false, true)).await;
    }
}
