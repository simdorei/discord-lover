use super::{ActionContext, ActionExecutor};
use crate::{
    app_backend::AppServerTurnBackend, bridge_state::BridgeState, command_plan::CommandAction,
    dead_generation_recovery::RuntimeDeadGenerationFence, queue_runner::QueueCoordinator,
    soak::native_fixture,
};
use cdr_app_server::ResidentAppServer;
use cdr_store::{
    ingress::{
        self, IngressKind, NewIngress,
        stop::{StopScope, accept_nonrunning},
    },
    queue::{self, NewQueueJob},
};
use serde_json::{Value, json};
use std::{future::Future, path::PathBuf, sync::Arc, task::Poll, time::Duration};

struct Fixture {
    _temp: tempfile::TempDir,
    db: PathBuf,
    log: PathBuf,
    server: Arc<ResidentAppServer>,
    executor: ActionExecutor<AppServerTurnBackend>,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_mode("normal").await
    }

    async fn with_mode(mode: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("store.sqlite");
        let state = temp.path().join("state.sqlite");
        let log = temp.path().join("rpc.jsonl");
        rusqlite::Connection::open(&state)
            .unwrap()
            .execute_batch(include_str!("../../../tests/fixtures/action_state.sql"))
            .unwrap();
        cdr_store::mapping::upsert_thread(&db, "thread-b", "p", "A", 100, 42, 1.0).unwrap();
        cdr_store::mapping::upsert_thread(&db, "thread-a", "p", "B", 100, 43, 1.0).unwrap();
        let fence = Arc::new(
            RuntimeDeadGenerationFence::new(db.clone(), "ingress-origin".into(), None).unwrap(),
        );
        let mut config = native_fixture::config("settings");
        config.environment.insert(
            "SETTINGS_TEST_LOG".into(),
            log.to_string_lossy().into_owned(),
        );
        config
            .environment
            .insert("SETTINGS_TEST_MODE".into(), mode.into());
        let server = Arc::new(
            ResidentAppServer::start_with_dead_generation_fence(config, fence)
                .await
                .unwrap(),
        );
        queue::enqueue(
            &db,
            NewQueueJob {
                job_id: "original",
                target_thread_id: "thread-b",
                channel_id: 42,
                owner_user_id: Some(3),
                discord_message_id: Some(801),
                app_server_generation: i64::try_from(server.generation()).unwrap(),
                prompt: "original A",
                queued: true,
                ack_sent: true,
                created_at: 1.0,
            },
        )
        .unwrap();
        let backend = Arc::new(AppServerTurnBackend::new(server.clone()));
        let queue = Arc::new(QueueCoordinator::new(db.clone(), backend));
        let bridge = Arc::new(BridgeState::new(temp.path().join("bridge.json")));
        let executor =
            ActionExecutor::new(state, db.clone(), bridge, queue).with_server(server.clone());
        Self {
            _temp: temp,
            db,
            log,
            server,
            executor,
        }
    }

    fn admit(
        &self,
        event: u64,
        target: &str,
        channel: u64,
        model: &str,
    ) -> (String, CommandAction, ActionContext) {
        let action = CommandAction::Settings {
            reference: Some(target.into()),
            model: Some(model.into()),
            effort: None,
            speed: None,
        };
        let binding = self
            .executor
            .settings_resolver()
            .bind(&action, channel)
            .unwrap()
            .unwrap();
        let key = format!("message:{event}");
        let id = i64::try_from(event).unwrap();
        ingress::admit(
            &self.db,
            &NewIngress {
                ingress_id: key.clone(),
                kind: IngressKind::Message,
                event_id: Some(id),
                application_id: None,
                channel_id: i64::try_from(channel).unwrap(),
                owner_user_id: 3,
                source_message_id: Some(id),
                payload: json!({"version":1,"plan":{"Execute":action},"settings_binding":binding}),
                target_thread_id: Some(target.into()),
                canonical_owner: None,
                now: 1.0,
            },
        )
        .unwrap();
        ingress::begin_execution(&self.db, &key, "processing", Some(target), 2.0).unwrap();
        (
            key,
            action,
            ActionContext {
                channel_id: channel,
                user_id: 3,
                discord_message_id: Some(event),
                auto_queue_when_busy: false,
            },
        )
    }

    fn stop(&self) {
        let start = std::time::Instant::now();
        accept_nonrunning(&self.db,StopScope{target:"thread-b",channel:42,owner:3},
            &json!({"target":"thread-b","route":"Explicit","command":{"Stop":{"reference":"thread-b"}}}),
            None,||Ok(())).unwrap().unwrap();
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
}

fn count(calls: &[Value], method: &str, target: &str) -> usize {
    calls
        .iter()
        .filter(|call| call["method"] == method && call["params"]["threadId"] == target)
        .count()
}

#[tokio::test]
async fn settings_catalog_wait_cannot_revive_retired_ingress_after_restore() {
    use cdr_store::archived_cleanup as cleanup;
    let f = Fixture::with_mode("catalog-pause").await;
    let (key, action, context) = f.admit(901, "thread-b", 42, "model-b");
    let before = ingress::get(&f.db, &key).unwrap().unwrap();
    let mut old = Box::pin(
        f.executor
            .execute_with_ingress_context(action, context, &key),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !f._temp.path().join("catalog-ready").exists() {
            tokio::select! {
                result=&mut old => panic!("catalog must suspend: {result:?}"),
                ()=tokio::time::sleep(Duration::from_millis(10))=>{}
            }
        }
    })
    .await
    .unwrap();
    let state = rusqlite::Connection::open(f._temp.path().join("state.sqlite")).unwrap();
    state
        .execute_batch("UPDATE threads SET archived=1,archived_at=50 WHERE id='thread-b'")
        .unwrap();
    let intent = cleanup::prepare(&f.db, &cleanup::Prepare {
        channel:42,target:"thread-b",parent:100,guild:1,origin:&Value::Null,now:3.0,
    }, || Ok(json!({"archived":state.query_row("SELECT archived FROM threads WHERE id='thread-b'",[],|r|r.get::<_,bool>(0))?}).to_string())).unwrap();
    state
        .execute_batch("UPDATE threads SET archived=0 WHERE id='thread-b'")
        .unwrap();
    cleanup::transition(
        &f.db,
        &intent,
        "aborted_source_changed",
        "source restored",
        4.0,
    )
    .unwrap();
    std::fs::write(f._temp.path().join("catalog-release"), "release").unwrap();
    let result = old.await;
    assert!(result.is_err(), "old settings request unexpectedly resumed");
    assert_eq!(count(&f.calls(), "thread/resume", "thread-b"), 0);
    assert_eq!(count(&f.calls(), "thread/settings/update", "thread-b"), 0);
    assert_eq!(ingress::get(&f.db, &key).unwrap().unwrap(), before);
    assert_eq!(
        cleanup::ingress_operation(&f.db, &key).unwrap(),
        Some(intent.operation_id)
    );
    let (key, action, context) = f.admit(902, "thread-b", 42, "model-b");
    let fresh = f
        .executor
        .execute_with_ingress_context(action, context, &key)
        .await;
    let calls = f.calls();
    f.server.close().await.unwrap();
    assert!(fresh.is_ok(), "{fresh:?}");
    assert_eq!(count(&calls, "thread/settings/update", "thread-b"), 1);
}

#[tokio::test]
async fn admitted_settings_after_stop_cannot_borrow_current_rpc_revision() {
    let f = Fixture::new().await;
    let (key, action, context) = f.admit(901, "thread-b", 42, "model-b");
    let before = ingress::get(&f.db, &key).unwrap().unwrap();
    f.stop();
    let result = f
        .executor
        .execute_with_ingress_context(action, context, &key)
        .await;
    let calls = f.calls();
    f.server.close().await.unwrap();
    assert!(
        result.is_err(),
        "old durable settings must not become a fresh post-stop RPC"
    );
    assert_eq!(count(&calls, "thread/resume", "thread-b"), 0);
    assert_eq!(count(&calls, "thread/settings/update", "thread-b"), 0);
    assert_eq!(ingress::get(&f.db, &key).unwrap().unwrap(), before);
    assert!(
        cdr_store::execution_hold::reason(&f.db, "original")
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn settings_waiting_for_target_lock_keeps_its_origin_and_fresh_both_targets_work() {
    let f = Fixture::new().await;
    let (key, action, context) = f.admit(901, "thread-b", 42, "model-b");
    let guard = f.executor.control_lock("thread-b").await.unwrap();
    let mut original = Box::pin(
        f.executor
            .execute_with_ingress_context(action, context, &key),
    );
    std::future::poll_fn(|cx| {
        assert!(
            matches!(original.as_mut().poll(cx), Poll::Pending),
            "target lock must suspend the original"
        );
        Poll::Ready(())
    })
    .await;
    f.stop();
    drop(guard);
    let old = original.await;
    let before_fresh = f.calls();
    let (b_key, b_action, b_context) = f.admit(902, "thread-a", 43, "model-b");
    let b = tokio::time::timeout(
        Duration::from_secs(5),
        f.executor
            .execute_with_ingress_context(b_action, b_context, &b_key),
    )
    .await
    .unwrap();
    let (a_key, a_action, a_context) = f.admit(903, "thread-b", 42, "model-a");
    let fresh = f
        .executor
        .execute_with_ingress_context(a_action, a_context, &a_key)
        .await;
    let all = f.calls();
    let healthy = !f.server.lifecycle_snapshot().await.quarantined;
    f.server.close().await.unwrap();
    assert!(old.is_err());
    assert_eq!(count(&before_fresh, "thread/resume", "thread-b"), 0);
    assert_eq!(
        count(&before_fresh, "thread/settings/update", "thread-b"),
        0
    );
    assert!(b.is_ok());
    assert!(fresh.is_ok());
    assert_eq!(count(&all, "thread/settings/update", "thread-a"), 1);
    assert_eq!(count(&all, "thread/settings/update", "thread-b"), 1);
    assert_eq!(count(&all, "turn/start", "thread-b"), 0);
    assert!(healthy);
    assert!(
        cdr_store::execution_hold::reason(&f.db, "original")
            .unwrap()
            .is_some()
    );
}
