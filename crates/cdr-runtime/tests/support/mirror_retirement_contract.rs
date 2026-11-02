use super::*;
use cdr_store::archived_cleanup as cleanup;
use std::time::Duration;

async fn wait_prepared(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if cleanup::get(path, 31).unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn admitted_dispatch_drains_and_restore_before_delete_preserves_room() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let sync = Arc::new(sync);
    let db = temp.path().join("mirror.sqlite");
    let permit = cleanup::effects::admit(&db, Some(31), Some("thread-old")).unwrap();
    let owner = sync.clone();
    let task = tokio::spawn(async move { owner.sync(99, None).await });
    wait_prepared(&db).await;
    assert_eq!(remote.0.lock().unwrap().deletes, 0);
    assert!(cleanup::effects::admit(&db, Some(31), Some("thread-old")).is_err());
    Connection::open(temp.path().join("state.sqlite"))
        .unwrap()
        .execute_batch("UPDATE threads SET archived=0 WHERE id='thread-old'")
        .unwrap();
    drop(permit);
    let result = task.await.unwrap().unwrap();
    assert_eq!(result.archived, 0);
    assert!(!result.skipped.is_empty());
    assert_eq!(remote.0.lock().unwrap().deletes, 0);
    assert_eq!(
        cleanup::get(&db, 31).unwrap().unwrap().phase,
        "aborted_source_changed"
    );
    upsert_project(&db, "C:/repos/old", "old", 21, 1.0, |a, b| a == b).unwrap();
    sync.sync(99, None).await.unwrap();
    assert_eq!(thread_channels(&db, "thread-old").unwrap(), Some((21, 31)));
    // A later explicit sync after a genuine rearchive creates another operation.
    Connection::open(temp.path().join("state.sqlite"))
        .unwrap()
        .execute_batch("UPDATE threads SET archived=1 WHERE id='thread-old'")
        .unwrap();
    assert_eq!(sync.sync(99, None).await.unwrap().archived, 1);
    assert_eq!(remote.0.lock().unwrap().deletes, 2); // thread and now-empty project
}

#[tokio::test]
async fn restore_after_delete_boundary_finishes_old_mapping_then_creates_new_room() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let db = temp.path().join("mirror.sqlite");
    let state = temp.path().join("state.sqlite");
    let original = state.clone();
    remote.0.lock().unwrap().before_delete = Some(Arc::new(move |room| {
        if room == 31 {
            Connection::open(&original)
                .unwrap()
                .execute_batch("UPDATE threads SET archived=0 WHERE id='thread-old'")
                .unwrap();
        }
    }));
    assert_eq!(sync.sync(99, None).await.unwrap().archived, 1);
    assert!(thread_channels(&db, "thread-old").unwrap().is_none());
    remote.0.lock().unwrap().before_delete = None;
    sync.sync(99, None).await.unwrap();
    assert_ne!(thread_channels(&db, "thread-old").unwrap().unwrap().1, 31);
    assert_eq!(
        cleanup::get(&db, 31).unwrap().unwrap().phase,
        "mapping_retired"
    );
}

#[tokio::test]
async fn unknown_room_is_excluded_while_other_archived_room_and_active_thread_sync() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let db = temp.path().join("mirror.sqlite");
    let intent = cleanup::prepare(
        &db,
        &cleanup::Prepare {
            channel: 31,
            target: "thread-old",
            parent: 21,
            guild: 1,
            origin: &serde_json::Value::Null,
            now: 2.0,
        },
        || Ok("{}".into()),
    )
    .unwrap();
    let attempted =
        cleanup::transition(&db, &intent, "delete_attempted", "prior process", 3.0).unwrap();
    cleanup::transition(&db, &attempted, "delete_unknown", "lost", 4.0).unwrap();
    Connection::open(temp.path().join("state.sqlite"))
        .unwrap()
        .execute_batch("UPDATE threads SET archived=1,archived_at=50 WHERE id='thread-a'")
        .unwrap();
    let result = sync.sync(99, None).await.unwrap();
    assert_eq!(result.archived, 1);
    assert_eq!(result.threads, 1);
    assert!(!result.skipped.is_empty());
    assert!(result.to_string().contains("partial"));
    let state = remote.0.lock().unwrap();
    assert!(state.channels.contains_key(&31));
    assert!(!state.channels.contains_key(&30));
    assert!(thread_channels(&db, "thread-old").unwrap().is_some());
}

#[tokio::test]
async fn preloaded_senders_and_native_effects_cannot_dispatch_after_retirement() {
    use cdr_app_server::DeadGenerationFence;
    use cdr_discord::http::DiscordHttp;
    use cdr_runtime::dead_generation_recovery::RuntimeDeadGenerationFence;
    use twilight_model::{
        http::interaction::{InteractionResponse, InteractionResponseType},
        id::Id,
    };
    let temp = tempfile::tempdir().unwrap();
    let (_sync, _remote) = fixture(&temp);
    let db = temp.path().join("mirror.sqlite");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        twilight_http::Client::builder()
            .token("fixture-token".into())
            .proxy(listener.local_addr().unwrap().to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let api = DiscordHttp::new(http.clone(), Id::new(1)).with_store(&db, Some(31));
    let fence = RuntimeDeadGenerationFence::new(db.clone(), "runtime".into(), None).unwrap();
    cleanup::prepare(
        &db,
        &cleanup::Prepare {
            channel: 31,
            target: "thread-old",
            parent: 21,
            guild: 1,
            origin: &serde_json::Value::Null,
            now: 2.0,
        },
        || Ok("{}".into()),
    )
    .unwrap();
    assert!(
        api.send_idempotent_message(Id::new(31), "old", &[], "domain", "key", 0)
            .await
            .is_err()
    );
    assert!(
        api.update_initial_response("original-interaction", "old")
            .await
            .is_err()
    );
    assert!(
        api.acknowledge(
            Id::new(1),
            "original-interaction",
            &InteractionResponse {
                kind: InteractionResponseType::DeferredChannelMessageWithSource,
                data: None
            }
        )
        .await
        .is_err()
    );
    for method in [
        "thread/resume",
        "turn/start",
        "turn/steer",
        "server/response",
        "thread/unsubscribe",
    ] {
        assert!(
            fence
                .admit_effect(method, &serde_json::json!({"threadId":"thread-old"}))
                .is_err(),
            "{method}"
        );
    }
    assert!(
        fence
            .admit_effect("turn/start", &serde_json::json!({"threadId":"thread-b"}))
            .is_ok()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}
