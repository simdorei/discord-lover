use super::*;

fn remove_old(temp: &tempfile::TempDir) {
    Connection::open(temp.path().join("state.sqlite"))
        .unwrap()
        .execute("DELETE FROM threads WHERE id='thread-old'", [])
        .unwrap();
}

#[tokio::test]
async fn shared_room_preserves_both_mappings_and_reports_conflict_without_pending_work() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    remove_old(&temp);
    let db = temp.path().join("mirror.sqlite");
    // Legacy duplicate mapping remains reachable when the active rollout is absent.
    Connection::open(temp.path().join("state.sqlite"))
        .unwrap()
        .execute(
            "UPDATE threads SET rollout_path='missing-active.jsonl' WHERE id='thread-a'",
            [],
        )
        .unwrap();
    upsert_thread(&db, "thread-a", "C:/repos/alpha", "active", 21, 31, 1.0).unwrap();
    let error = sync.sync(99, None).await.unwrap().to_string();
    assert!(error.contains("shared Discord room 31"), "{error}");
    assert_eq!(thread_channels(&db, "thread-old").unwrap(), Some((21, 31)));
    assert_eq!(thread_channels(&db, "thread-a").unwrap(), Some((21, 31)));
    assert!(remote.0.lock().unwrap().channels.contains_key(&31));
    assert_eq!(cdr_store::room_cleanup::phase(&db, 31).unwrap(), None);
    // Refusal must not fence the surviving active room or destroy late requests.
    let incoming = cdr_store::ingress::NewIngress {
        ingress_id: "message:shared".into(),
        kind: cdr_store::ingress::IngressKind::Message,
        event_id: Some(55),
        application_id: None,
        channel_id: 31,
        owner_user_id: 42,
        source_message_id: Some(55),
        payload: serde_json::json!({"content":"preserve shared request"}),
        target_thread_id: Some("thread-old".into()),
        canonical_owner: None,
        now: 2.0,
    };
    let saved = cdr_store::ingress::admit(&db, &incoming)
        .unwrap()
        .record
        .unwrap();
    assert_eq!(saved.payload, incoming.payload);
    assert_eq!(saved.state, "staged");
    assert!(!sync.sync(99, None).await.unwrap().skipped.is_empty());
    assert_eq!(thread_channels(&db, "thread-old").unwrap(), Some((21, 31)));
    assert_eq!(thread_channels(&db, "thread-a").unwrap(), Some((21, 31)));
    assert!(remote.0.lock().unwrap().channels.contains_key(&31));
    assert_eq!(
        cdr_store::ingress::get(&db, "message:shared")
            .unwrap()
            .unwrap()
            .payload,
        incoming.payload
    );
}

#[tokio::test]
async fn full_sync_deletes_mapping_and_room_for_absent_codex_thread() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    remove_old(&temp);
    let result = sync.sync(99, None).await.unwrap();
    assert_eq!(result.archived, 1);
    assert!(!remote.0.lock().unwrap().channels.contains_key(&31));
    assert!(
        thread_channels(&temp.path().join("mirror.sqlite"), "thread-old")
            .unwrap()
            .is_none()
    );
    assert!(remote.0.lock().unwrap().channels.contains_key(&30));
    assert_eq!(sync.sync(99, None).await.unwrap().archived, 0);
}

#[tokio::test]
async fn limited_sync_preserves_absent_codex_mapping() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    remove_old(&temp);
    assert_eq!(sync.sync(99, Some(1)).await.unwrap().archived, 0);
    assert!(remote.0.lock().unwrap().channels.contains_key(&31));
    assert!(
        thread_channels(&temp.path().join("mirror.sqlite"), "thread-old")
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn missing_rollout_is_not_absent_database_identity() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    Connection::open(temp.path().join("state.sqlite"))
        .unwrap()
        .execute(
            "UPDATE threads SET archived=0,rollout_path='not-present.jsonl' WHERE id='thread-old'",
            [],
        )
        .unwrap();
    let result = sync.sync(99, None).await.unwrap();
    assert_eq!(result.unavailable, 1);
    assert_eq!(result.archived, 0);
    assert!(remote.0.lock().unwrap().channels.contains_key(&31));
}

#[tokio::test]
async fn absent_thread_with_pending_request_is_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    remove_old(&temp);
    cdr_store::queue::enqueue(
        &temp.path().join("mirror.sqlite"),
        cdr_store::queue::NewQueueJob {
            job_id: "pending-old",
            target_thread_id: "thread-old",
            channel_id: 31,
            owner_user_id: None,
            discord_message_id: None,
            app_server_generation: 1,
            prompt: "preserve me",
            queued: true,
            ack_sent: false,
            created_at: 1.0,
        },
    )
    .unwrap();
    let error = sync.sync(99, None).await.unwrap().to_string();
    assert!(error.contains("queued requests"), "{error}");
    assert!(remote.0.lock().unwrap().channels.contains_key(&31));
    assert!(
        thread_channels(&temp.path().join("mirror.sqlite"), "thread-old")
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn failed_state_inventory_does_not_delete_rooms() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    Connection::open(temp.path().join("state.sqlite"))
        .unwrap()
        .execute("ALTER TABLE threads RENAME TO unavailable_threads", [])
        .unwrap();
    assert!(sync.sync(99, None).await.is_err());
    assert!(remote.0.lock().unwrap().channels.contains_key(&30));
    assert!(remote.0.lock().unwrap().channels.contains_key(&31));
}

#[tokio::test]
async fn reappearing_identity_during_discord_lookup_is_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    remove_old(&temp);
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    remote.0.lock().unwrap().pause_channel = Some((31, started.clone(), release.clone()));
    let task = tokio::spawn(async move { sync.sync(99, None).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    Connection::open(temp.path().join("state.sqlite")).unwrap().execute(
        "INSERT INTO threads (id,title,cwd,updated_at,rollout_path,archived,source,thread_source) VALUES ('thread-old','restored','C:/repos/old',99,'restored.jsonl',0,'vscode','user')", []
    ).unwrap();
    release.notify_one();
    assert_eq!(task.await.unwrap().unwrap().archived, 0);
    assert!(remote.0.lock().unwrap().channels.contains_key(&31));
    assert!(
        thread_channels(&temp.path().join("mirror.sqlite"), "thread-old")
            .unwrap()
            .is_some()
    );
}
