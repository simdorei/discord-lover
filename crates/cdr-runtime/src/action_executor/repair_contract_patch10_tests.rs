use super::*;
use crate::{
    app_backend::AppServerTurnBackend,
    bridge_state::BridgeState,
    command_plan::CommandAction,
    dead_generation_recovery::RuntimeDeadGenerationFence,
    test_support::{message_fixture::MessageFixture, native_fixture},
};
use std::{collections::BTreeSet, path::Path, time::Instant};

const RUNTIME: &str = "repair-contract-runtime";

#[tokio::test]
async fn archive_fence_created_while_waiting_for_target_lock_blocks_reset() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = Arc::new(fixture(&temp, "ready").await);
    let guard = fixture
        .executor
        .freeze_recovery(42, &CommandAction::Repair { reference: None })
        .unwrap();
    let lock = fixture
        .queue
        .target_lock("thread-b")
        .unwrap()
        .lock_owned()
        .await;
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let task = tokio::spawn({
        let fixture = fixture.clone();
        async move {
            entered.send(()).unwrap();
            fixture.executor.repair_tools_bound(guard).await
        }
    });
    waiting.await.unwrap();
    assert_eq!(calls(&temp, "rpc.jsonl").len(), 1);
    let db = fixture.executor.mirror_db();
    let reservation =
        cdr_store::archive_fence::reserve(db, &BTreeSet::from(["thread-b".to_owned()]), None)
            .unwrap();
    drop(lock);
    assert!(
        task.await.unwrap().is_err(),
        "repair ignored archive committed during target-lock wait"
    );
    assert_eq!(resets(&temp, "rpc.jsonl"), 0);
    let operation: String = cdr_store::schema::open_initialized(db)
        .unwrap()
        .query_row(
            "SELECT operation_id FROM codex_archive_fences WHERE target_thread_id='thread-b'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(operation, reservation.operation_id);
    fixture.server.close().await.unwrap();
}

#[tokio::test]
async fn archive_schema_read_failure_is_closed_before_tool_effect() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(&temp, "ready").await;
    let guard = fixture
        .executor
        .freeze_recovery(42, &CommandAction::Repair { reference: None })
        .unwrap();
    // Deliberately incompatible schema in this disposable database only.
    cdr_store::schema::open_initialized(fixture.executor.mirror_db())
        .unwrap()
        .execute_batch(
            "DROP TABLE codex_archive_fences; CREATE TABLE codex_archive_fences (broken INTEGER);",
        )
        .unwrap();
    assert!(fixture.executor.repair_tools_bound(guard).await.is_err());
    assert_eq!(resets(&temp, "rpc.jsonl"), 0);
    fixture.server.close().await.unwrap();
}

async fn server(
    temp: &tempfile::TempDir,
    mode: &str,
    name: &str,
    runtime: &str,
) -> Arc<ResidentAppServer> {
    let database = temp.path().join("mirror.sqlite");
    let fence =
        Arc::new(RuntimeDeadGenerationFence::new(database, runtime.into(), Some(42)).unwrap());
    let mut config = native_fixture::config("repair-contract");
    config.environment.insert(
        "CDR_REPAIR_RPC_LOG".into(),
        temp.path().join(name).to_string_lossy().into(),
    );
    config
        .environment
        .insert("CDR_REPAIR_MODE".into(), mode.into());
    let server = Arc::new(
        ResidentAppServer::start_with_dead_generation_fence(config, fence)
            .await
            .unwrap(),
    );
    crate::idle_release::install(&server, &temp.path().join("mirror.sqlite")).unwrap();
    server
}

async fn fixture(temp: &tempfile::TempDir, mode: &str) -> MessageFixture {
    let server = server(temp, mode, "rpc.jsonl", RUNTIME).await;
    MessageFixture::with_server(
        temp,
        Arc::new(twilight_http::Client::new("unused-fixture".into())),
        server,
    )
}

fn cold_executor(
    temp: &tempfile::TempDir,
    server: Arc<ResidentAppServer>,
) -> ActionExecutor<AppServerTurnBackend> {
    let database = temp.path().join("mirror.sqlite");
    let backend = Arc::new(AppServerTurnBackend::new(server.clone()));
    ActionExecutor::new(
        temp.path().join("state.sqlite"),
        database.clone(),
        Arc::new(BridgeState::new(temp.path().join("bridge.json"))),
        Arc::new(crate::queue_runner::QueueCoordinator::new(
            database, backend,
        )),
    )
    .with_server(server)
}

fn calls(temp: &tempfile::TempDir, name: &str) -> Vec<Value> {
    std::fs::read_to_string(temp.path().join(name))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn resets(temp: &tempfile::TempDir, name: &str) -> usize {
    calls(temp, name)
        .iter()
        .filter(|v| v["params"]["tool"] == "js_reset")
        .count()
}

fn receipt(temp: &tempfile::TempDir) -> (PathBuf, Vec<u8>, Value) {
    let entries: Vec<_> = std::fs::read_dir(temp.path().join("maintenance_backups/tool-repair"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    let bytes = std::fs::read(&entries[0]).unwrap();
    let value = serde_json::from_slice(&bytes).unwrap();
    (entries[0].clone(), bytes, value)
}

async fn other(server: &ResidentAppServer, release_reset: bool) -> Value {
    let started = Instant::now();
    server.request_for_tool_repair("mcpServer/tool/call",
        json!({"threadId":"thread-a","server":"node_repl","tool":"js","arguments":{"code":"void 0"}}),
        Duration::from_secs(2), server.generation()).await.unwrap();
    let value = server
        .request(
            "thread/read",
            json!({"threadId":"thread-a","fixtureReleaseReset":release_reset}),
            Duration::from_secs(2),
            Some(server.generation()),
        )
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "healthy other target lost progress"
    );
    value["thread"].clone()
}

async fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "fixture did not reach {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_for_reset(temp: &tempfile::TempDir) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if resets(temp, "rpc.jsonl") == 1 {
            return;
        }
        assert!(Instant::now() < deadline, "reset was not dispatched");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn failed_stage(mode: &str, stage: &str, count: usize) {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(&temp, mode).await;
    let error = fixture
        .executor
        .repair_tools(42, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("\u{b3c4}\u{ad6c} \u{bcf5}\u{ad6c} \u{c644}\u{b8cc}"));
    let (_, _, saved) = receipt(&temp);
    assert_eq!(saved["phase"], "failed", "{mode}: {saved}");
    assert_eq!(saved["stage"], stage, "{mode}: {saved}");
    assert!(error.contains(stage), "{mode}: {error}");
    assert_eq!(
        calls(&temp, "rpc.jsonl")
            .iter()
            .filter(|v| v["method"] == "mcpServer/tool/call")
            .count(),
        count
    );
    assert_eq!(fixture.server.generation(), 1);
    assert!(!fixture.server.lifecycle_snapshot().await.quarantined);
    assert!(
        cdr_store::queue::list(fixture.executor.mirror_db())
            .unwrap()
            .is_empty()
    );
    fixture.server.close().await.unwrap();
}

#[tokio::test]
async fn verified_repair_preserves_identifiable_other_session() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(&temp, "ready").await;
    let before = other(&fixture.server, false).await;
    assert_eq!(before["fixtureSession"], "protected-other-session");
    let result = fixture.executor.repair_tools(42, None).await.unwrap();
    assert!(
        result
            .text
            .contains("\u{b3c4}\u{ad6c} \u{bcf5}\u{ad6c} \u{c644}\u{b8cc}")
    );
    assert_eq!(other(&fixture.server, false).await, before);
    let effects: Vec<_> = calls(&temp, "rpc.jsonl")
        .into_iter()
        .filter(|v| v["method"] == "mcpServer/tool/call" && v["params"]["threadId"] == "thread-b")
        .collect();
    assert_eq!(effects.len(), 3);
    assert!(
        effects
            .iter()
            .all(|v| v["params"]["threadId"] == "thread-b")
    );
    assert_eq!(effects[0]["params"]["tool"], "js_reset");
    assert_eq!(effects[1]["params"]["arguments"]["code"], INIT);
    assert_eq!(effects[2]["params"]["arguments"]["code"], PROBE);
    let (path, _, saved) = receipt(&temp);
    assert_eq!(saved["phase"], "verified");
    assert_eq!(saved["stage"], "probe");
    assert_eq!(
        saved["operation_id"],
        path.file_stem().unwrap().to_str().unwrap()
    );
    assert_eq!(saved["server_instance"], fixture.server.instance_id());
    assert_eq!(saved["generation"], fixture.server.generation());
    assert_eq!(saved["app_restarted"], false);
    fixture.server.close().await.unwrap();
}

#[tokio::test]
async fn malformed_initialization_result_cannot_be_reported_verified() {
    for mode in ["init-malformed", "init-no-content"] {
        failed_stage(mode, "initialize", 2).await;
    }
}

#[tokio::test]
async fn reset_error_and_format_failure_have_durable_stage() {
    for mode in ["reset-error", "reset-format"] {
        failed_stage(mode, "reset", 1).await;
    }
}

#[tokio::test]
async fn initialization_error_has_durable_stage() {
    failed_stage("init-error", "initialize", 2).await;
}

#[tokio::test]
async fn probe_error_and_format_failure_have_durable_stage() {
    for mode in ["probe-error", "probe-format", "pipe"] {
        failed_stage(mode, "probe", 3).await;
    }
}

#[tokio::test]
async fn flushed_reset_cancel_survives_cold_queue_new_instance_and_late_ack() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(&temp, "late-reset").await;
    let original_instance = fixture.server.instance_id().to_owned();
    let before = other(&fixture.server, false).await;
    let execution = cold_executor(&temp, fixture.server.clone());
    let task = tokio::spawn(async move { execution.repair_tools(42, None).await });
    wait_for_reset(&temp).await;
    assert_eq!(other(&fixture.server, false).await, before);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let (original_path, original_bytes, original_receipt) = receipt(&temp);
    assert_eq!(original_receipt["phase"], "tool_call_pending");
    let db = fixture.executor.mirror_db();
    assert!(cdr_store::mutation_attempt::check(db, RUNTIME, Some("thread-b")).is_err());
    assert!(cdr_store::mutation_attempt::check(db, RUNTIME, Some("thread-a")).is_ok());
    assert!(
        cold_executor(&temp, fixture.server.clone())
            .repair_tools(42, None)
            .await
            .is_err()
    );
    assert_eq!(resets(&temp, "rpc.jsonl"), 1);
    assert_eq!(std::fs::read(&original_path).unwrap(), original_bytes);
    assert_eq!(other(&fixture.server, true).await, before);
    assert!(!fixture.server.lifecycle_snapshot().await.quarantined);
    let replacement = server(&temp, "ready", "replacement.jsonl", "repair-cold-runtime").await;
    assert_ne!(replacement.instance_id(), original_instance);
    assert_eq!(replacement.generation(), 1);
    // The original child is demonstrably still alive. A new instance/generation is not exit proof.
    let old_observation = fixture
        .server
        .request(
            "thread/read",
            json!({"threadId":"thread-a"}),
            Duration::from_secs(2),
            Some(fixture.server.generation()),
        )
        .await
        .unwrap();
    assert_eq!(old_observation["thread"], before);
    assert!(
        cold_executor(&temp, replacement.clone())
            .repair_tools(42, None)
            .await
            .is_err()
    );
    assert_eq!(resets(&temp, "replacement.jsonl"), 0);
    assert_eq!(std::fs::read(&original_path).unwrap(), original_bytes);
    assert_eq!(
        other(&replacement, false).await["fixtureSession"],
        "protected-other-session"
    );
    replacement.close().await.unwrap();
    fixture.server.close().await.unwrap();
}

#[tokio::test]
async fn reset_timeout_is_target_scoped_and_survives_cold_coordinator() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(&temp, "timeout").await;
    let before = other(&fixture.server, false).await;
    assert!(fixture.executor.repair_tools(42, None).await.is_err());
    let (path, bytes, saved) = receipt(&temp);
    assert_eq!(saved["phase"], "outcome_unknown");
    assert_eq!(saved["stage"], "reset");
    assert!(
        cdr_store::mutation_attempt::check(fixture.executor.mirror_db(), RUNTIME, Some("thread-b"))
            .is_err()
    );
    assert!(!fixture.server.lifecycle_snapshot().await.quarantined);
    assert_eq!(other(&fixture.server, false).await, before);
    assert!(
        cold_executor(&temp, fixture.server.clone())
            .repair_tools(42, None)
            .await
            .is_err()
    );
    assert_eq!(resets(&temp, "rpc.jsonl"), 1);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    fixture.server.close().await.unwrap();
}

#[tokio::test]
async fn pending_starting_and_running_requests_survive_repair_refusal() {
    for state in ["pending", "starting", "running"] {
        let temp = tempfile::tempdir().unwrap();
        let fixture = fixture(&temp, "ready").await;
        let db = fixture.executor.mirror_db();
        cdr_store::queue::enqueue(
            db,
            cdr_store::queue::NewQueueJob {
                job_id: "original",
                target_thread_id: "thread-b",
                channel_id: 42,
                owner_user_id: Some(3),
                discord_message_id: Some(77),
                app_server_generation: 1,
                prompt: "preserve original",
                queued: true,
                ack_sent: true,
                created_at: 1.0,
            },
        )
        .unwrap();
        if state != "pending" {
            cdr_store::queue::try_begin_attempt(db, "original", &[], 1)
                .unwrap()
                .unwrap();
        }
        if state == "running" {
            cdr_store::queue::mark_running(db, "original", "original-turn", 1).unwrap();
        }
        let before = cdr_store::queue::list(db).unwrap();
        assert!(fixture.executor.repair_tools(42, None).await.is_err());
        assert_eq!(cdr_store::queue::list(db).unwrap(), before);
        assert_eq!(resets(&temp, "rpc.jsonl"), 0);
        fixture.server.close().await.unwrap();
    }
}

#[tokio::test]
async fn archive_attempted_and_verified_fences_are_not_bypassed_or_removed() {
    for verified in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let fixture = fixture(&temp, "ready").await;
        let db = fixture.executor.mirror_db();
        let reservation =
            cdr_store::archive_fence::reserve(db, &BTreeSet::from(["thread-b".to_owned()]), None)
                .unwrap();
        if verified {
            cdr_store::archive_fence::verified(db, &reservation).unwrap();
        }
        assert!(cdr_store::archive_fence::target_is_fenced(db, "thread-b").unwrap());
        let result = fixture.executor.repair_tools(42, None).await;
        assert!(result.is_err(), "repair bypassed archive fence: {result:?}");
        assert_eq!(resets(&temp, "rpc.jsonl"), 0);
        assert!(cdr_store::archive_fence::target_is_fenced(db, "thread-b").unwrap());
        fixture.server.close().await.unwrap();
    }
}

#[tokio::test]
async fn receipt_creation_failure_prevents_first_tool_effect() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(&temp, "ready").await;
    let dir = temp.path().join("maintenance_backups");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("tool-repair"), b"preserved blocking evidence").unwrap();
    assert!(fixture.executor.repair_tools(42, None).await.is_err());
    assert_eq!(resets(&temp, "rpc.jsonl"), 0);
    assert_eq!(
        std::fs::read(dir.join("tool-repair")).unwrap(),
        b"preserved blocking evidence"
    );
    assert!(
        cdr_store::queue::list(fixture.executor.mirror_db())
            .unwrap()
            .is_empty()
    );
    fixture.server.close().await.unwrap();
}

#[tokio::test]
async fn admitted_stop_during_repair_preflight_revokes_old_reset_without_erasing_stop() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = Arc::new(fixture(&temp, "gate-inventory").await);
    let db = fixture.executor.mirror_db();
    let _repair = fixture.admit_id("!repair", 801);
    assert!(
        cdr_store::ingress::begin_execution(
            db,
            "message:801",
            "processing",
            None,
            std::time::SystemTime::UNIX_EPOCH
                .elapsed()
                .unwrap()
                .as_secs_f64()
        )
        .unwrap()
    );
    let task = tokio::spawn({
        let fixture = fixture.clone();
        async move {
            fixture
                .executor
                .execute_with_ingress_context(
                    CommandAction::Repair { reference: None },
                    crate::action_executor::ActionContext {
                        channel_id: 42,
                        user_id: 3,
                        discord_message_id: Some(801),
                        auto_queue_when_busy: false,
                    },
                    "message:801",
                )
                .await
        }
    });
    wait_for(&temp.path().join("rpc.jsonl.entered")).await;
    let started = Instant::now();
    let _stop = fixture.admit_id("!stop", 802);
    assert!(
        cdr_store::ingress::begin_execution(
            db,
            "message:802",
            "processing",
            None,
            std::time::SystemTime::UNIX_EPOCH
                .elapsed()
                .unwrap()
                .as_secs_f64()
        )
        .unwrap()
    );
    let acceptance = fixture
        .executor
        .execute_with_ingress_context(
            CommandAction::Stop { reference: None },
            crate::action_executor::ActionContext {
                channel_id: 42,
                user_id: 3,
                discord_message_id: Some(802),
                auto_queue_when_busy: false,
            },
            "message:802",
        )
        .await
        .unwrap();
    assert!(acceptance.text.contains("Stop accepted"));
    let stop = cdr_store::ingress::get(db, "message:802").unwrap().unwrap();
    assert_eq!(stop.phase, "stop_accepted");
    let admission_elapsed = started.elapsed();
    std::fs::write(temp.path().join("rpc.jsonl.release"), b"continue").unwrap();
    let result = task.await.unwrap();
    assert!(
        admission_elapsed < Duration::from_secs(3),
        "stop admission waited for repair"
    );
    assert!(
        result.is_err(),
        "old repair survived a newer admitted stop: {result:?}"
    );
    assert_eq!(resets(&temp, "rpc.jsonl"), 0);
    assert_eq!(
        cdr_store::ingress::get(db, "message:802").unwrap(),
        Some(stop.clone())
    );
    assert!(
        cdr_store::ingress::get(db, "message:801")
            .unwrap()
            .is_some()
    );
    assert!(cdr_store::queue::list(db).unwrap().is_empty());
    fixture.server.close().await.unwrap();
}
