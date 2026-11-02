use super::*;

#[tokio::test]
async fn cleanup_preserves_foreign_threads_and_limited_scope_orphans() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    {
        let mut state = remote.0.lock().unwrap();
        state.channels.insert(
            32,
            channel(32, Some(20), ChannelType::PublicThread, "human thread"),
        );
        state.channels.insert(
            33,
            channel(33, Some(20), ChannelType::PublicThread, "bot orphan"),
        );
        state.foreign_ids.insert(32);
    }
    sync.sync(99, Some(1)).await.unwrap();
    assert!(remote.0.lock().unwrap().channels.contains_key(&33));
    sync.sync(99, None).await.unwrap();
    let state = remote.0.lock().unwrap();
    assert!(state.channels.contains_key(&32));
    assert!(state.channels.contains_key(&30));
    assert!(!state.channels.contains_key(&33));
}

#[tokio::test]
async fn failed_delete_keeps_the_mapping_for_retry() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    remote.0.lock().unwrap().fail_delete = Some(31);
    assert!(
        sync.sync(99, None)
            .await
            .unwrap()
            .to_string()
            .contains("HTTP 403")
    );
    assert!(
        thread_channels(&temp.path().join("mirror.sqlite"), "thread-old")
            .unwrap()
            .is_some()
    );
    assert!(remote.0.lock().unwrap().channels.contains_key(&31));
    remote.0.lock().unwrap().fail_delete = None;
    sync.sync(99, None).await.unwrap();
    assert!(!remote.0.lock().unwrap().channels.contains_key(&31));
}

#[tokio::test]
async fn orphan_with_pending_requests_is_not_deleted_and_sync_reports_why() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    remote.0.lock().unwrap().channels.insert(
        32,
        channel(32, Some(20), ChannelType::PublicThread, "orphan"),
    );
    cdr_store::queue::enqueue(
        &temp.path().join("mirror.sqlite"),
        cdr_store::queue::NewQueueJob {
            job_id: "preserve-job",
            target_thread_id: "preserve-target",
            channel_id: 32,
            owner_user_id: None,
            discord_message_id: None,
            app_server_generation: 1,
            prompt: "preserve",
            queued: true,
            ack_sent: false,
            created_at: 1.0,
        },
    )
    .unwrap();
    assert!(
        sync.sync(99, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("queued requests")
    );
    assert!(remote.0.lock().unwrap().channels.contains_key(&32));
    assert_eq!(
        cdr_store::queue::list(&temp.path().join("mirror.sqlite"))
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn empty_stale_project_is_removed_after_its_threads() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let db = temp.path().join("mirror.sqlite");
    upsert_project(&db, "C:/repos/old", "old", 21, 1.0, |a, b| a == b).unwrap();
    let result = sync.sync(99, None).await.unwrap();
    assert_eq!(result.projects_deleted, 1);
    assert!(!remote.0.lock().unwrap().channels.contains_key(&21));
    assert!(
        cdr_store::mapping::project_for_channel(&db, Some(21))
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn orphan_with_intake_ingress_outbox_or_unknown_receipt_is_preserved() {
    for (kind, sql) in [
        (
            "prompt intake",
            "INSERT INTO codex_prompt_intakes (job_id,target_thread_id,channel_id,raw_prompt,auto_queue_when_busy,require_current_mirror,created_at,updated_at) VALUES ('intake','target',32,'pending',1,1,1,1)",
        ),
        (
            "ingress",
            "INSERT INTO discord_ingress_journal (ingress_id,kind,channel_id,owner_user_id,payload_json,state,phase,created_at,updated_at) VALUES ('ingress','message',32,1,'{}','held','processing',1,1)",
        ),
        (
            "undelivered result",
            "INSERT INTO codex_delivery_outbox (delivery_id,job_id,target_thread_id,turn_id,channel_id,content,created_at,updated_at) VALUES ('delivery','job','target','turn',32,'final',1,1)",
        ),
        (
            "delivery receipt",
            "INSERT INTO codex_delivery_receipts (receipt_key,content_hash) VALUES ('[32,\"final\",\"job\",0]','hash')",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (sync, remote) = fixture(&temp);
        remote.0.lock().unwrap().channels.insert(
            32,
            channel(32, Some(20), ChannelType::PublicThread, "protected orphan"),
        );
        let db = temp.path().join("mirror.sqlite");
        Connection::open(&db).unwrap().execute_batch(sql).unwrap();
        let error = sync.sync(99, None).await.unwrap_err().to_string();
        assert!(error.contains(kind), "{kind}: {error}");
        assert!(remote.0.lock().unwrap().channels.contains_key(&32));
    }
}

#[tokio::test]
async fn settled_or_other_room_receipts_do_not_block_cleanup() {
    for sql in [
        "INSERT INTO codex_delivery_receipts (receipt_key,content_hash,message_id) VALUES ('[32,\"final\",\"job\",0]','hash','confirmed')",
        "INSERT INTO codex_delivery_receipts (receipt_key,content_hash) VALUES ('[999,\"final\",\"job\",0]','hash')",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (sync, remote) = fixture(&temp);
        remote.0.lock().unwrap().channels.insert(
            32,
            channel(32, Some(20), ChannelType::PublicThread, "orphan"),
        );
        Connection::open(temp.path().join("mirror.sqlite"))
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        sync.sync(99, None).await.unwrap();
        assert!(!remote.0.lock().unwrap().channels.contains_key(&32));
    }
}
