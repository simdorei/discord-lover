use super::*;

#[tokio::test]
async fn authoritative_missing_room_keeps_evidence_and_finishes_only_its_mapping() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let db = temp.path().join("mirror.sqlite");
    let key = rejection(&db, 801, &no_dispatch());
    let before = ingress::get(&db, &key).unwrap().unwrap();
    remote.0.lock().unwrap().channels.remove(&31);
    assert_eq!(sync.sync(99, None).await.unwrap().archived, 1);
    assert_eq!(ingress::get(&db, &key).unwrap().unwrap(), before);
    assert_eq!(audit_count(&db), 1);
    assert_eq!(
        cdr_store::archived_cleanup::get(&db, 31)
            .unwrap()
            .map(|i| i.phase)
            .as_deref(),
        Some("mapping_retired")
    );
    assert!(thread_channels(&db, "thread-old").unwrap().is_none());
}

#[tokio::test]
async fn completion_write_failure_cannot_authorize_a_second_delete() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let db = temp.path().join("mirror.sqlite");
    rejection(&db, 801, &no_dispatch());
    Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER completion_failure BEFORE UPDATE ON cdr_archived_room_dispositions
         WHEN NEW.phase='deleted' BEGIN SELECT RAISE(ABORT,'injected completion failure'); END;",
        )
        .unwrap();
    let deletes = Arc::new(AtomicUsize::new(0));
    let count = deletes.clone();
    remote.0.lock().unwrap().before_delete = Some(Arc::new(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
    }));
    assert!(!sync.sync(99, None).await.unwrap().skipped.is_empty());
    assert!(!remote.0.lock().unwrap().channels.contains_key(&31));
    assert!(thread_channels(&db, "thread-old").unwrap().is_some());
    assert_eq!(
        cdr_store::archived_cleanup::get(&db, 31)
            .unwrap()
            .map(|i| i.phase)
            .as_deref(),
        Some("delete_attempted")
    );
    assert_eq!(audit_count(&db), 1);
    assert!(!sync.sync(99, None).await.unwrap().skipped.is_empty());
    assert_eq!(deletes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ingress_arriving_after_archived_close_is_saved_but_cannot_execute() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let db = temp.path().join("mirror.sqlite");
    rejection(&db, 801, &no_dispatch());
    let capture = db.clone();
    remote.0.lock().unwrap().before_delete = Some(Arc::new(move |room| {
        if room == 31 {
            let request = ingress::NewIngress {
                ingress_id: "message:900".into(),
                kind: ingress::IngressKind::Message,
                event_id: Some(900),
                application_id: None,
                channel_id: 31,
                owner_user_id: 42,
                source_message_id: Some(900),
                payload: json!({"text":"late"}),
                target_thread_id: Some("thread-old".into()),
                canonical_owner: None,
                now: 30.0,
            };
            ingress::admit(&capture, &request).unwrap();
        }
    }));
    sync.sync(99, None).await.unwrap();
    let late = ingress::get(&db, "message:900").unwrap().unwrap();
    assert_eq!(late.state, "held");
    assert_eq!(late.phase, "cleanup_retired");
    assert!(!ingress::begin_execution(&db, "message:900", "processing", None, 31.0).unwrap());
    assert_eq!(late.payload["text"], "late");
}

#[tokio::test]
async fn mirror_scope_keeps_mapped_internal_threads_without_importing_unmapped_ones() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, _) = fixture(&temp);
    Connection::open(temp.path().join("state.sqlite"))
        .unwrap()
        .execute_batch(
            "UPDATE threads SET source='app-server',thread_source='subagent' WHERE archived=0",
        )
        .unwrap();
    let text = sync.inspect(99, None, true).await.unwrap();
    assert!(text.contains("expected_threads: 1"), "{text}");
    assert!(!text.contains("missing_mapping | thread-b"), "{text}");
    assert_eq!(sync.sync(99, None).await.unwrap().threads, 1);
    assert_eq!(
        thread_channels(&temp.path().join("mirror.sqlite"), "thread-a").unwrap(),
        Some((20, 30))
    );
    assert!(
        thread_channels(&temp.path().join("mirror.sqlite"), "thread-b")
            .unwrap()
            .is_none()
    );
}
