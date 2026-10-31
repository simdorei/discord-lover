use super::{ActionContext, ActionExecutor};
use crate::{
    app_backend::AppServerTurnBackend, bridge_state::BridgeState, command_plan::CommandAction,
    dead_generation_recovery::RuntimeDeadGenerationFence, queue_runner::QueueCoordinator,
    soak::native_fixture,
};
use cdr_app_server::{
    AppServerError, DeadGenerationFence, DeadGenerationWork, RequestId, ResidentAppServer,
};
use cdr_store::{
    ingress::{
        self, IngressKind, NewIngress,
        stop::{StopScope, accept_nonrunning},
    },
    queue::{self, NewQueueJob},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

struct ArchiveFence {
    inner: RuntimeDeadGenerationFence,
    db: PathBuf,
    stop_at_archive: Option<String>,
    fired: AtomicBool,
}

fn stop_original(db: &std::path::Path, target: &str, generation: u64) {
    let generation = i64::try_from(generation).unwrap();
    queue::enqueue(
        db,
        NewQueueJob {
            job_id: "stop-original",
            target_thread_id: target,
            channel_id: 42,
            owner_user_id: Some(3),
            discord_message_id: Some(801),
            app_server_generation: generation,
            prompt: "original input",
            queued: true,
            ack_sent: true,
            created_at: 1.0,
        },
    )
    .unwrap();
    accept_nonrunning(
        db,
        StopScope {
            target,
            channel: 42,
            owner: 3,
        },
        &json!({"target":target,"route":"Explicit","command":{"Stop":{"reference":target}}}),
        None,
        || Ok(()),
    )
    .unwrap()
    .unwrap();
    // The real recovery cancellation removes the queue row, not the stop evidence.
    // Thus archive preflight cannot substitute queue presence for the writer check.
    queue::cancel_for_recovery(db, target, 42, 3, 3.0).unwrap();
    assert!(
        cdr_store::execution_hold::reason(db, "stop-original")
            .unwrap()
            .is_some()
    );
}

fn stop_unowned(db: &std::path::Path, target: &str) {
    let original = ingress::admit(
        db,
        &NewIngress {
            ingress_id: "message:802".into(),
            kind: IngressKind::Message,
            event_id: Some(802),
            application_id: None,
            channel_id: 42,
            owner_user_id: 3,
            source_message_id: Some(802),
            payload: json!({"version":1,"plan":{"Execute":{"Ask":{"prompt":"late held input"}}}}),
            target_thread_id: Some(target.into()),
            canonical_owner: None,
            now: 3.0,
        },
    )
    .unwrap()
    .record
    .unwrap();
    let receipt = accept_nonrunning(
        db,
        StopScope {
            target,
            channel: 42,
            owner: 3,
        },
        &json!({"target":target,"route":"Explicit","command":{"Stop":{"reference":target}}}),
        None,
        || Ok(()),
    )
    .unwrap()
    .unwrap();
    assert_eq!(receipt.ingresses, ["message:802"]);
    assert!(receipt.jobs.is_empty());
    let saved = ingress::get(db, "message:802").unwrap().unwrap();
    assert_eq!(saved.payload, original.payload);
    assert_eq!(saved.state, "held");
    if !original.hold_reason.is_empty() {
        assert_eq!(saved.hold_reason, original.hold_reason);
    }
}

impl DeadGenerationFence for ArchiveFence {
    fn persist(&self, work: &DeadGenerationWork) -> Result<(), AppServerError> {
        self.inner.persist(work)
    }
    fn check_request(
        &self,
        generation: u64,
        method: &str,
        params: &Value,
    ) -> Result<(), AppServerError> {
        self.inner.check_request(generation, method, params)
    }
    fn request_origin(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<Option<Value>, AppServerError> {
        self.inner.request_origin(method, params)
    }
    fn begin_mutation(
        &self,
        owner: (&str, u64),
        request: (&str, &RequestId),
        method: &str,
        params: &Value,
        scoped: bool,
    ) -> Result<bool, AppServerError> {
        self.inner
            .begin_mutation(owner, request, method, params, scoped)
    }
    fn begin_mutation_with_origin(
        &self,
        owner: (&str, u64),
        request: (&str, &RequestId),
        method: &str,
        params: &Value,
        scoped: bool,
        origin: Option<&Value>,
    ) -> Result<bool, AppServerError> {
        if method == "thread/archive"
            && let Some(target) = &self.stop_at_archive
            && !self.fired.swap(true, Ordering::AcqRel)
        {
            stop_unowned(&self.db, target);
        }
        self.inner
            .begin_mutation_with_origin(owner, request, method, params, scoped, origin)
    }
    fn finish_mutation(
        &self,
        owner: (&str, u64),
        request: (&str, &RequestId),
        outcome: &str,
    ) -> Result<(), AppServerError> {
        self.inner.finish_mutation(owner, request, outcome)
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    db: PathBuf,
    state: PathBuf,
    log: PathBuf,
    server: Arc<ResidentAppServer>,
    fence: Arc<ArchiveFence>,
    executor: ActionExecutor<AppServerTurnBackend>,
}

impl Fixture {
    async fn new(scenario: &str, stop_at_archive: Option<&str>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("store.sqlite");
        let state = temp.path().join("state.sqlite");
        let log = temp.path().join("rpc.jsonl");
        let state_db = rusqlite::Connection::open(&state).unwrap();
        state_db
            .execute_batch(include_str!("../../../tests/fixtures/action_state.sql"))
            .unwrap();
        state_db.execute("INSERT INTO threads SELECT 'child','child',cwd,updated_at,rollout_path,model,reasoning_effort,tokens_used,0,0,source,thread_source FROM threads WHERE id='thread-b'",[]).unwrap();
        cdr_store::mapping::upsert_thread(&db, "thread-b", "p", "Parent", 100, 42, 1.0).unwrap();
        let inner =
            RuntimeDeadGenerationFence::new(db.clone(), "archive-origin".into(), None).unwrap();
        let fence = Arc::new(ArchiveFence {
            inner,
            db: db.clone(),
            stop_at_archive: stop_at_archive.map(str::to_owned),
            fired: AtomicBool::new(false),
        });
        let mut config = native_fixture::config("archive");
        for (key, value) in [
            ("ARCHIVE_STATE", state.to_string_lossy().into_owned()),
            ("ARCHIVE_LOG", log.to_string_lossy().into_owned()),
            ("ARCHIVE_SCENARIO", scenario.into()),
        ] {
            config.environment.insert(key.into(), value);
        }
        let server = Arc::new(
            ResidentAppServer::start_with_dead_generation_fence(config, fence.clone())
                .await
                .unwrap(),
        );
        let queue = Arc::new(QueueCoordinator::new(
            db.clone(),
            Arc::new(AppServerTurnBackend::new(server.clone())),
        ));
        let bridge = Arc::new(BridgeState::new(temp.path().join("bridge.json")));
        let executor = ActionExecutor::new(state.clone(), db.clone(), bridge, queue)
            .with_server(server.clone());
        Self {
            _temp: temp,
            db,
            state,
            log,
            server,
            fence,
            executor,
        }
    }

    fn admit(&self) -> (String, CommandAction, ActionContext) {
        let action = CommandAction::Archive {
            reference: Some("thread-b".into()),
        };
        let binding = self
            .executor
            .settings_resolver()
            .bind_lifecycle(&action, 42)
            .unwrap()
            .unwrap();
        let key = "message:901".to_owned();
        ingress::admit(
            &self.db,
            &NewIngress {
                ingress_id: key.clone(),
                kind: IngressKind::Message,
                event_id: Some(901),
                application_id: None,
                channel_id: 42,
                owner_user_id: 3,
                source_message_id: Some(901),
                payload: json!({"version":1,"content":"!archive thread-b","plan":{"Execute":action},
                "lifecycle_binding":binding}),
                target_thread_id: Some("thread-b".into()),
                canonical_owner: None,
                now: 1.0,
            },
        )
        .unwrap();
        ingress::begin_execution(&self.db, &key, "processing", Some("thread-b"), 2.0).unwrap();
        (
            key,
            action,
            ActionContext {
                channel_id: 42,
                user_id: 3,
                discord_message_id: Some(901),
                auto_queue_when_busy: false,
            },
        )
    }

    fn calls(&self, method: &str, target: &str) -> usize {
        std::fs::read_to_string(&self.log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|call| call["method"] == method && call["params"]["threadId"] == target)
            .count()
    }

    fn archived(&self) -> i64 {
        rusqlite::Connection::open(&self.state)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM threads WHERE id IN ('thread-b','child') AND archived=1",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }
}

#[tokio::test]
async fn bound_archive_verifies_descendant_persistence_without_resuming() {
    let f = Fixture::new("descendant_normal", None).await;
    let (key, action, context) = f.admit();
    let result = f
        .executor
        .execute_with_ingress_context(action, context, &key)
        .await;
    f.server.close().await.unwrap();
    assert!(
        result.is_ok(),
        "normal bound descendant archive must succeed: {result:?}"
    );
    assert_eq!(f.calls("thread/resume", "thread-b"), 0);
    assert_eq!(f.calls("thread/resume", "child"), 0);
    assert_eq!(f.calls("thread/archive", "thread-b"), 1);
    assert_eq!(f.archived(), 2);
    assert_eq!(f.calls("turn/start", "thread-b"), 0);
    assert_eq!(f.calls("turn/start", "child"), 0);
}

#[tokio::test]
async fn root_or_child_stop_after_admission_cannot_be_recaptured() {
    for target in ["thread-b", "child"] {
        let f = Fixture::new("descendant_normal", None).await;
        let (key, action, context) = f.admit();
        let original = ingress::get(&f.db, &key).unwrap().unwrap();
        stop_original(&f.db, target, f.server.generation());
        let result = f
            .executor
            .execute_with_ingress_context(action, context, &key)
            .await;
        f.server.close().await.unwrap();
        assert!(result.is_err(), "{target}: {result:?}");
        assert_eq!(f.calls("thread/resume", "child"), 0);
        assert_eq!(f.calls("thread/archive", "thread-b"), 0);
        assert_eq!(f.archived(), 0);
        assert_eq!(ingress::get(&f.db, &key).unwrap().unwrap(), original);
    }
}

#[tokio::test]
async fn final_archive_writer_rechecks_both_root_and_verified_child_revision() {
    for target in ["thread-b", "child"] {
        let f = Fixture::new("descendant_normal", Some(target)).await;
        let (key, action, context) = f.admit();
        let result = f
            .executor
            .execute_with_ingress_context(action, context, &key)
            .await;
        f.server.close().await.unwrap();
        assert!(f.fence.fired.load(Ordering::Acquire));
        assert!(result.is_err(), "{target}: {result:?}");
        assert_eq!(f.calls("thread/resume", "child"), 0);
        assert_eq!(f.calls("thread/archive", "thread-b"), 0);
        assert_eq!(f.archived(), 0);
        assert!(cdr_store::archive_fence::target_is_fenced(&f.db, "thread-b").unwrap());
        assert!(cdr_store::archive_fence::target_is_fenced(&f.db, "child").unwrap());
    }
}

#[tokio::test]
async fn changed_or_invalid_descendant_scope_still_refuses_archive() {
    for scenario in ["descendant_changed", "scope_root"] {
        let f = Fixture::new(scenario, None).await;
        let (key, action, context) = f.admit();
        let result = f
            .executor
            .execute_with_ingress_context(action, context, &key)
            .await;
        f.server.close().await.unwrap();
        assert!(result.is_err(), "{scenario}: {result:?}");
        assert_eq!(f.calls("thread/archive", "thread-b"), 0);
        assert_eq!(f.archived(), 0);
    }
}
