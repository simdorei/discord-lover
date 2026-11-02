use super::*;

#[tokio::test]
async fn vanished_project_preserves_earlier_ingress_and_fences_late_ingress() {
    for request_first in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let (sync, remote) = fixture(&temp);
        let db = temp.path().join("mirror.sqlite");
        upsert_project(&db, "C:/repos/old", "old", 21, 1.0, |a, b| a == b).unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        remote.0.lock().unwrap().pause_channel = Some((21, started.clone(), release.clone()));
        let job = tokio::spawn(async move { sync.sync(99, None).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        remote.0.lock().unwrap().channels.remove(&21);
        let incoming = cdr_store::ingress::NewIngress {
            ingress_id: "message:55".into(),
            kind: cdr_store::ingress::IngressKind::Message,
            event_id: Some(55),
            application_id: None,
            channel_id: 21,
            owner_user_id: 42,
            source_message_id: Some(55),
            payload: serde_json::json!({"content":"프로젝트 원문 보존"}),
            target_thread_id: None,
            canonical_owner: None,
            now: 2.0,
        };
        if request_first {
            cdr_store::ingress::admit(&db, &incoming).unwrap();
        }
        release.notify_one();
        let result = job.await.unwrap();
        if request_first {
            assert!(result.unwrap_err().to_string().contains("ingress"));
            let rows: i64 = Connection::open(&db)
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM mirror_projects WHERE discord_channel_id=21",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(rows, 1);
            assert_eq!(
                cdr_store::ingress::get(&db, "message:55")
                    .unwrap()
                    .unwrap()
                    .state,
                "staged"
            );
        } else {
            result.unwrap();
            let saved = cdr_store::ingress::admit(&db, &incoming)
                .unwrap()
                .record
                .unwrap();
            assert_eq!(saved.state, "held");
            assert_eq!(saved.payload, incoming.payload);
            assert!(
                !cdr_store::ingress::begin_execution(&db, "message:55", "processing", None, 3.0)
                    .unwrap()
            );
        }
    }
}

#[tokio::test]
async fn request_arriving_during_delete_is_durably_held_and_never_executable() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let db = temp.path().join("mirror.sqlite");
    let callback_db = db.clone();
    remote.0.lock().unwrap().before_delete = Some(Arc::new(move |channel| {
        if channel != 31 {
            return;
        }
        let admission = cdr_store::ingress::admit(
            &callback_db,
            &cdr_store::ingress::NewIngress {
                ingress_id: "message:12345".into(),
                kind: cdr_store::ingress::IngressKind::Message,
                event_id: Some(12345),
                application_id: None,
                channel_id: 31,
                owner_user_id: 42,
                source_message_id: Some(12345),
                payload: serde_json::json!({"content":"지우지 말고 요청을 보존"}),
                target_thread_id: Some("thread-old".into()),
                canonical_owner: None,
                now: 2.0,
            },
        )
        .unwrap();
        assert_eq!(admission.record.unwrap().state, "held");
        assert!(
            !cdr_store::ingress::begin_execution(
                &callback_db,
                "message:12345",
                "processing",
                Some("thread-old"),
                3.0
            )
            .unwrap()
        );
    }));
    sync.sync(99, None).await.unwrap();
    let saved = cdr_store::ingress::get(&db, "message:12345")
        .unwrap()
        .unwrap();
    assert_eq!(saved.state, "held");
    assert_eq!(saved.payload["content"], "지우지 말고 요청을 보존");
    assert!(saved.hold_reason.contains("cleanup"));
}

#[tokio::test]
async fn cancelled_delete_keeps_fence_across_reopen_and_never_retries() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let sync = Arc::new(sync);
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    remote.0.lock().unwrap().pause_delete = Some((31, started.clone(), release));
    let running = sync.clone();
    let job = tokio::spawn(async move { running.sync(99, None).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    let db = temp.path().join("mirror.sqlite");
    assert_eq!(
        cdr_store::archived_cleanup::get(&db, 31)
            .unwrap()
            .map(|i| i.phase)
            .as_deref(),
        Some("delete_attempted")
    );
    remote.0.lock().unwrap().pause_delete = None;
    assert!(
        sync.sync(99, None)
            .await
            .unwrap()
            .to_string()
            .contains("unknown")
    );
    assert!(remote.0.lock().unwrap().channels.contains_key(&31));
}

#[tokio::test]
async fn lost_delete_response_keeps_mapping_and_reports_uncertainty_on_next_sync() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    remote.0.lock().unwrap().lose_delete_response = Some(31);
    assert!(
        sync.sync(99, None)
            .await
            .unwrap()
            .to_string()
            .contains("unknown")
    );
    let db = temp.path().join("mirror.sqlite");
    assert!(thread_channels(&db, "thread-old").unwrap().is_some());
    assert_eq!(sync.sync(99, None).await.unwrap().archived, 1);
    assert!(thread_channels(&db, "thread-old").unwrap().is_none());
    assert_eq!(remote.0.lock().unwrap().deletes, 1);
}
