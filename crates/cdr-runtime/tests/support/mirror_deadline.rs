use super::*;
use std::time::Duration;
use tokio::{
    sync::Notify,
    task::JoinHandle,
    time::{Instant, timeout},
};

fn pause_project(remote: &Remote) -> Arc<Notify> {
    let started = Arc::new(Notify::new());
    remote.0.lock().unwrap().pause_channel = Some((20, started.clone(), Arc::new(Notify::new())));
    started
}

async fn operation_error<T: std::fmt::Debug>(
    mut task: JoinHandle<Result<T, MirrorSyncError>>,
) -> MirrorSyncError {
    if let Ok(result) = timeout(Duration::from_secs(125), &mut task).await {
        result.unwrap().unwrap_err()
    } else {
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        panic!("whole mirror operation exceeded its fixed 125s test watchdog");
    }
}

#[tokio::test(start_paused = true)]
async fn mirror_deadline_all_entry_lock_waits_are_bounded_without_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let sync = Arc::new(sync);
    let started = pause_project(&remote);
    let owner = sync.clone();
    let task = tokio::spawn(async move { owner.sync(99, Some(1)).await });
    timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    let began = Instant::now();
    let outcomes = timeout(Duration::from_secs(12), async {
        tokio::join!(
            async { sync.sync(99, Some(1)).await.map(|_| ()) },
            async { sync.inspect(99, Some(1), false).await.map(|_| ()) },
            async {
                sync.link_new_thread(20, "new-thread", "hello", None)
                    .await
                    .map(|_| ())
            },
            sync.retire_exact_absent("missing", 999, 20),
        )
    })
    .await;
    assert!(
        !task.is_finished(),
        "waiting callers must not cancel the lock owner"
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let (sync_result, inspect_result, new_result, cleanup_result) =
        outcomes.expect("mirror lock wait exceeded its fixed 12s test watchdog");
    assert_eq!(
        began.elapsed(),
        Duration::from_secs(10),
        "virtual lock budget"
    );
    for outcome in [sync_result, inspect_result, new_result, cleanup_result] {
        let message = outcome.unwrap_err().to_string();
        assert!(message.contains("phase=lock_wait"), "{message}");
        assert!(message.contains("deadline=10s"), "{message}");
        assert!(message.contains("not started"), "{message}");
    }
    assert_eq!(remote.0.lock().unwrap().creates, 0);
    assert_eq!(
        thread_channels(&temp.path().join("mirror.sqlite"), "thread-a").unwrap(),
        Some((20, 30))
    );
}

#[tokio::test(start_paused = true)]
async fn mirror_deadline_whole_sync_preserves_partial_and_unknown_creation() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let sync = Arc::new(sync);
    let started = Arc::new(Notify::new());
    remote.0.lock().unwrap().pause_create = Some((
        ChannelType::PublicThread,
        started.clone(),
        Arc::new(Notify::new()),
    ));
    let owner = sync.clone();
    let began = Instant::now();
    let task = tokio::spawn(async move { owner.sync(99, None).await });
    timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    let error = operation_error(task).await.to_string();
    assert_eq!(
        began.elapsed(),
        Duration::from_mins(2),
        "virtual whole-operation budget"
    );
    assert!(
        error.contains("phase=operation") && error.contains("deadline=120s"),
        "{error}"
    );
    assert!(
        error.contains("earlier changes may have completed"),
        "{error}"
    );
    assert_eq!(remote.0.lock().unwrap().creates, 2);
    let path = temp.path().join("mirror.sqlite");
    let db = Connection::open(&path).unwrap();
    assert!(
        cdr_store::mapping::project_for_channel(&path, Some(1001))
            .unwrap()
            .is_some(),
        "the project created and mapped before the stalled thread remains committed"
    );
    let phase: String = db
        .query_row(
            "SELECT phase FROM cdr_mirror_thread_creations WHERE thread_id='thread-b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(phase, "attempted");
    assert_eq!(thread_channels(&path, "thread-b").unwrap(), None);
    remote.0.lock().unwrap().pause_create = None;
    let cold = MirrorSynchronizer::new(
        temp.path().join("state.sqlite"),
        path.clone(),
        remote.clone(),
        Some(1),
    );
    assert!(cold.sync(99, None).await.is_err());
    assert_eq!(
        remote.0.lock().unwrap().creates,
        2,
        "no replay after the command timer expired"
    );
    assert!(remote.0.lock().unwrap().channels.contains_key(&1002));
}

#[tokio::test(start_paused = true)]
async fn mirror_deadline_readonly_inspection_timeout_does_not_write_mapping() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let path = temp.path().join("mirror.sqlite");
    let before = fs::read(&path).unwrap();
    let started = pause_project(&remote);
    let began = Instant::now();
    let task = tokio::spawn(async move { sync.inspect(99, Some(1), false).await });
    timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    let error = operation_error(task).await.to_string();
    assert_eq!(began.elapsed(), Duration::from_mins(2));
    assert!(
        error.contains("phase=operation") && error.contains("read-only"),
        "{error}"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(remote.0.lock().unwrap().creates, 0);
}

#[tokio::test]
async fn mirror_deadline_blocked_mirror_does_not_prevent_healthy_b_start() {
    use cdr_runtime::{
        action_executor::ActionContext, bridge_state::BridgeState, command_plan::CommandAction,
    };
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let started = pause_project(&remote);
    let path = temp.path().join("mirror.sqlite");
    upsert_thread(&path, "thread-b", "C:/repos/beta", "B", 21, 32, 2.0).unwrap();
    cdr_store::queue::mark_app_server_managed_target(&path, "thread-b", 7).unwrap();
    let backend = Arc::new(action_target::FakeBackend::default());
    let executor = action_target::executor(
        &temp,
        path.clone(),
        Arc::new(BridgeState::new(temp.path().join("bridge.json"))),
        backend.clone(),
    );
    let task = tokio::spawn(async move { sync.sync(99, Some(1)).await });
    timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let began = Instant::now();
    let result = timeout(
        Duration::from_secs(5),
        executor.execute_with_context(
            CommandAction::Ask {
                prompt: "B remains independent".into(),
            },
            ActionContext {
                channel_id: 32,
                user_id: 20,
                discord_message_id: Some(900),
                auto_queue_when_busy: true,
            },
        ),
    )
    .await;
    assert!(!task.is_finished());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    result
        .expect("healthy B exceeded unchanged 5s bound")
        .unwrap();
    assert!(began.elapsed() < Duration::from_secs(5));
    assert_eq!(
        backend.starts.lock().await.as_slice(),
        &[("thread-b".into(), "B remains independent".into())]
    );
    let jobs = cdr_store::queue::list(&path).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, cdr_store::queue::QueueJobState::Running);
    assert_eq!(remote.0.lock().unwrap().creates, 0);
}

#[tokio::test(start_paused = true)]
async fn mirror_deadline_total_budget_includes_lock_wait() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let sync = Arc::new(sync);
    let started_a = Arc::new(Notify::new());
    let release_a = Arc::new(Notify::new());
    remote.0.lock().unwrap().pause_channel = Some((20, started_a.clone(), release_a.clone()));
    let owner = sync.clone();
    let first = tokio::spawn(async move { owner.sync(99, Some(1)).await });
    timeout(Duration::from_secs(2), started_a.notified())
        .await
        .unwrap();
    let began = Instant::now();
    let waiter = sync.clone();
    let second = tokio::spawn(async move { waiter.inspect(99, Some(1), false).await });
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_secs(9)).await;
    let started_b = Arc::new(Notify::new());
    remote.0.lock().unwrap().pause_channel = Some((20, started_b.clone(), Arc::new(Notify::new())));
    release_a.notify_one();
    first.await.unwrap().unwrap();
    timeout(Duration::from_secs(2), started_b.notified())
        .await
        .unwrap();
    let error = operation_error(second).await.to_string();
    assert_eq!(
        began.elapsed(),
        Duration::from_mins(2),
        "lock wait must not renew the whole budget"
    );
    assert!(
        error.contains("phase=operation") && error.contains("including lock wait"),
        "{error}"
    );
}

#[tokio::test(start_paused = true)]
async fn mirror_deadline_deleted_room_reply_stall_keeps_fence_without_redelete() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    // Real OS storage preparation is outside the virtual Discord deadline clock.
    tokio::time::resume();
    cdr_runtime::agy::ownership::AgyOwnership::for_root(temp.path(), 1)
        .ensure_loaded(&temp.path().join("mirror.sqlite"))
        .await
        .unwrap();
    tokio::time::pause();
    let started = Arc::new(Notify::new());
    remote.0.lock().unwrap().pause_delete_after =
        Some((31, started.clone(), Arc::new(Notify::new())));
    let task = tokio::spawn(async move { sync.sync(99, None).await });
    timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    assert!(
        !remote.0.lock().unwrap().channels.contains_key(&31),
        "fixture deleted before withholding its reply"
    );
    let error = operation_error(task).await.to_string();
    assert!(error.contains("phase=operation"), "{error}");
    let path = temp.path().join("mirror.sqlite");
    assert_eq!(
        cdr_store::room_cleanup::phase(&path, 31)
            .unwrap()
            .as_deref(),
        Some("deleting")
    );
    assert_eq!(
        thread_channels(&path, "thread-old").unwrap(),
        Some((21, 31))
    );
    assert_eq!(remote.0.lock().unwrap().deletes, 1);
    remote.0.lock().unwrap().pause_delete_after = None;
    let cold = MirrorSynchronizer::new(
        temp.path().join("state.sqlite"),
        path.clone(),
        remote.clone(),
        Some(1),
    );
    assert!(cold.sync(99, None).await.is_err());
    assert_eq!(
        remote.0.lock().unwrap().deletes,
        1,
        "unknown deletion must not be dispatched again"
    );
    assert_eq!(
        cdr_store::room_cleanup::phase(&path, 31)
            .unwrap()
            .as_deref(),
        Some("deleting")
    );
}
