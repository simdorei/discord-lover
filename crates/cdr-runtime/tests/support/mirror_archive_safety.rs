use super::*;
use cdr_store::room_cleanup;
use std::sync::atomic::{AtomicUsize, Ordering};

fn assert_preserved(temp: &tempfile::TempDir, remote: &Remote) {
    let db = temp.path().join("mirror.sqlite");
    assert!(remote.0.lock().unwrap().channels.contains_key(&31));
    assert!(thread_channels(&db, "thread-old").unwrap().is_some());
    assert_eq!(room_cleanup::phase(&db, 31).unwrap(), None);
}

fn audit_count(db: &std::path::Path) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM cdr_archived_room_records WHERE table_name='discord_ingress_journal'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

#[tokio::test]
async fn native_archive_authorizes_cleanup_independently_of_old_outcomes() {
    for outcome in [
        json!({"kind":"busy_control_preflight_rejected","control_dispatched":true}),
        json!({"kind":"busy_control_preflight_rejected","control_dispatched":"false"}),
        json!({"kind":"busy_control_preflight_rejected","control_dispatched":0}),
        json!({"kind":"busy_control_preflight_rejected","control_dispatched":null}),
        json!({"kind":"other","control_dispatched":false}),
        json!({}),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (sync, remote) = fixture(&temp);
        rejection(&temp.path().join("mirror.sqlite"), 801, &outcome);
        let db = temp.path().join("mirror.sqlite");
        let before = ingress::get(&db, "interaction:801").unwrap().unwrap();
        assert_eq!(sync.sync(99, None).await.unwrap().archived, 1, "{outcome}");
        assert!(!remote.0.lock().unwrap().channels.contains_key(&31));
        assert_eq!(
            ingress::get(&db, "interaction:801").unwrap().unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn legacy_malformed_payloads_and_control_states_are_retained_without_veto() {
    for change in [
        "owner_user_id=43",
        "canonical_owner='busy-choice:foreign'",
        "owner_kind='prompt',owner_id='claimed'",
        "phase='processing'",
        "payload_json=json_set(payload_json,'$.busy_action','stop')",
        "payload_json=json_set(payload_json,'$.busy_choice.target_thread_id','other')",
        "payload_json=json_set(payload_json,'$.busy_choice.channel_id',99)",
        "payload_json=json_set(payload_json,'$.busy_choice.expires_at',9999999999)",
        "payload_json=json_set(payload_json,'$.busy_choice.allow_steer',json('true'))",
        "payload_json='malformed'",
        "outcome_json='malformed'",
        "target_thread_id=NULL",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (sync, remote) = fixture(&temp);
        let db = temp.path().join("mirror.sqlite");
        rejection(&db, 801, &no_dispatch());
        Connection::open(&db)
            .unwrap()
            .execute_batch(&format!(
                "UPDATE discord_ingress_journal SET {change} WHERE ingress_id='interaction:801'"
            ))
            .unwrap();
        let connection = Connection::open(&db).unwrap();
        let before:String=connection.query_row("SELECT json_array(payload_json,outcome_json,state,phase,owner_user_id,canonical_owner,owner_kind,owner_id,target_thread_id) FROM discord_ingress_journal WHERE ingress_id='interaction:801'",[],|r|r.get(0)).unwrap();
        assert_eq!(sync.sync(99, None).await.unwrap().archived, 1, "{change}");
        assert!(!remote.0.lock().unwrap().channels.contains_key(&31));
        let after:String=connection.query_row("SELECT json_array(payload_json,outcome_json,state,phase,owner_user_id,canonical_owner,owner_kind,owner_id,target_thread_id) FROM discord_ingress_journal WHERE ingress_id='interaction:801'",[],|r|r.get(0)).unwrap();
        assert_eq!(before, after);
        assert_eq!(audit_count(&db), 1);
    }
}

#[tokio::test]
async fn canonical_owner_receipt_is_preserved_with_retired_request() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let db = temp.path().join("mirror.sqlite");
    rejection(&db, 801, &no_dispatch());
    Connection::open(&db).unwrap().execute_batch(
        "INSERT INTO discord_ingress_owner_receipts (owner_key,owner_kind,owner_id,target_thread_id,channel_id,owner_user_id,payload_json,created_at)
         SELECT canonical_owner,'prompt','owned-job',target_thread_id,channel_id,owner_user_id,payload_json,1 FROM discord_ingress_journal",
    ).unwrap();
    assert_eq!(sync.sync(99, None).await.unwrap().archived, 1);
    assert!(!remote.0.lock().unwrap().channels.contains_key(&31));
    let count: i64 = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM discord_ingress_owner_receipts",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn all_old_pending_families_are_preserved_without_blocking_native_archive_cleanup() {
    for sql in [
        "INSERT INTO codex_prompt_intakes (job_id,target_thread_id,channel_id,raw_prompt,auto_queue_when_busy,require_current_mirror,created_at,updated_at) VALUES ('next','thread-old',99,'pending',1,1,1,1)",
        "INSERT INTO codex_commentary_outbox (delivery_key,job_id,target_thread_id,turn_id,channel_id,text) VALUES ('progress','job','thread-old','turn',31,'pending')",
        "INSERT INTO codex_goal_progress (thread,turn,channel,content) VALUES ('thread-old','turn',31,'pending')",
        "INSERT INTO busy_choices (choice_id,owner_user_id,channel_id,target_thread_id,prompt,allow_steer,created_at,expires_at) VALUES ('live',42,31,'thread-old','pending',0,1,unixepoch()+600)",
        "INSERT INTO codex_delivery_receipts (receipt_key,content_hash) VALUES ('[31,\"later\",\"job\",0]','hash')",
        "INSERT INTO codex_delivery_receipts (receipt_key,content_hash) VALUES ('broken','hash')",
        "INSERT INTO codex_delivery_outbox (delivery_id,job_id,target_thread_id,turn_id,channel_id,content,created_at,updated_at) VALUES ('pending','job','thread-old','turn',31,'pending',1,1)",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (sync, remote) = fixture(&temp);
        let db = temp.path().join("mirror.sqlite");
        rejection(&db, 801, &no_dispatch());
        Connection::open(&db).unwrap().execute_batch(sql).unwrap();
        assert_eq!(sync.sync(99, None).await.unwrap().archived, 1, "{sql}");
        assert!(!remote.0.lock().unwrap().channels.contains_key(&31));
        assert_eq!(audit_count(&db), 1);
    }
}

#[tokio::test]
async fn slow_lookup_rechecks_ingress_proof_parent_and_source() {
    for change in ["late-ingress", "proof", "parent", "source", "rollout"] {
        let temp = tempfile::tempdir().unwrap();
        let (sync, remote) = fixture(&temp);
        let db = temp.path().join("mirror.sqlite");
        rejection(&db, 801, &no_dispatch());
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        remote.0.lock().unwrap().pause_channel = Some((31, started.clone(), release.clone()));
        let worker = tokio::spawn(async move { sync.sync(99, None).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        let connection = Connection::open(&db).unwrap();
        match change {
            "late-ingress" => {
                connection.execute_batch(
                "INSERT INTO discord_ingress_journal (ingress_id,kind,channel_id,owner_user_id,payload_json,state,phase,target_thread_id,created_at,updated_at) VALUES ('late','message',99,42,'{}','staged','admitted','thread-old',1,1)",
            ).unwrap();
            }
            "proof" => {
                connection
                    .execute_batch("UPDATE discord_ingress_journal SET outcome_json='{}'")
                    .unwrap();
            }
            "parent" => {
                connection.execute_batch("UPDATE mirror_threads SET discord_channel_id=99 WHERE codex_thread_id='thread-old'").unwrap();
            }
            "source" => {
                Connection::open(temp.path().join("state.sqlite"))
                    .unwrap()
                    .execute_batch("UPDATE threads SET archived=0 WHERE id='thread-old'")
                    .unwrap();
            }
            "rollout" => {
                fs::remove_file(temp.path().join("rollout.jsonl")).unwrap();
            }
            _ => unreachable!(),
        }
        release.notify_one();
        let result = worker.await.unwrap().unwrap();
        if matches!(change, "parent" | "source") {
            assert!(!result.skipped.is_empty(), "{change}");
            assert_preserved(&temp, &remote);
            assert_eq!(audit_count(&db), 0);
        } else {
            assert_eq!(result.archived, 1, "{change}");
            assert!(!remote.0.lock().unwrap().channels.contains_key(&31));
            assert_eq!(
                audit_count(&db),
                if change == "late-ingress" { 2 } else { 1 }
            );
        }
    }
}

#[tokio::test]
async fn wrong_remote_parent_guild_or_identity_never_deletes() {
    for mode in ["parent", "guild", "id", "type"] {
        let temp = tempfile::tempdir().unwrap();
        let (sync, remote) = fixture(&temp);
        rejection(&temp.path().join("mirror.sqlite"), 801, &no_dispatch());
        {
            let mut state = remote.0.lock().unwrap();
            let channel = state.channels.get_mut(&31).unwrap();
            match mode {
                "parent" => channel.parent_id = Some(20),
                "guild" => channel.guild_id = Some(2),
                "id" => channel.id = 999,
                "type" => channel.kind = ChannelType::GuildText,
                _ => unreachable!(),
            }
        }
        assert!(
            !sync.sync(99, None).await.unwrap().skipped.is_empty(),
            "{mode}"
        );
        assert_preserved(&temp, &remote);
    }
}

#[tokio::test]
async fn audit_write_failure_rolls_back_fence_before_delete() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let db = temp.path().join("mirror.sqlite");
    rejection(&db, 801, &no_dispatch());
    Connection::open(&db).unwrap().execute_batch(
        "CREATE TRIGGER audit_failure BEFORE INSERT ON cdr_archived_room_records BEGIN SELECT RAISE(ABORT,'injected audit failure'); END;",
    ).unwrap();
    assert!(!sync.sync(99, None).await.unwrap().skipped.is_empty());
    assert_preserved(&temp, &remote);
    assert_eq!(audit_count(&db), 0);
}

#[tokio::test]
async fn uncertain_delete_is_not_retried_and_rejection_only_releases_its_fence() {
    for lost in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (sync, remote) = fixture(&temp);
        let db = temp.path().join("mirror.sqlite");
        let key = rejection(&db, 801, &no_dispatch());
        let before = ingress::get(&db, &key).unwrap().unwrap();
        let deletes = Arc::new(AtomicUsize::new(0));
        {
            let mut state = remote.0.lock().unwrap();
            if lost {
                state.lose_delete_response = Some(31);
            } else {
                state.fail_delete = Some(31);
            }
            let count = deletes.clone();
            state.before_delete = Some(Arc::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
            }));
        }
        assert!(!sync.sync(99, None).await.unwrap().skipped.is_empty());
        assert!(thread_channels(&db, "thread-old").unwrap().is_some());
        assert_eq!(audit_count(&db), 1);
        assert_eq!(ingress::get(&db, &key).unwrap().unwrap(), before);
        if lost {
            assert_eq!(
                cdr_store::archived_cleanup::get(&db, 31)
                    .unwrap()
                    .map(|i| i.phase)
                    .as_deref(),
                Some("delete_unknown")
            );
            assert_eq!(sync.sync(99, None).await.unwrap().archived, 1);
            assert!(thread_channels(&db, "thread-old").unwrap().is_none());
            assert_eq!(deletes.load(Ordering::SeqCst), 1);
        } else {
            assert_preserved(&temp, &remote);
        }
    }
}

#[tokio::test]
async fn confirmed_cleanup_audits_original_and_recovery_does_not_post_again() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, _) = fixture(&temp);
    let db = temp.path().join("mirror.sqlite");
    let key = rejection(&db, 801, &no_dispatch());
    Connection::open(&db)
        .unwrap()
        .execute_batch("UPDATE discord_ingress_journal SET notice_staged=0")
        .unwrap();
    let before = ingress::get(&db, &key).unwrap().unwrap();
    sync.sync(99, None).await.unwrap();
    ingress::recover_prior_runtime(&db, "new-runtime", 100.0).unwrap();
    assert_eq!(ingress::get(&db, &key).unwrap().unwrap(), before);
    let connection = Connection::open(&db).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM codex_delivery_outbox", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let (payload, outcome, snapshot): (String, String, String) = connection
        .query_row(
            "SELECT json_extract(snapshot_json,'$.payload_json'),json_extract(snapshot_json,'$.outcome_json'),snapshot_json FROM cdr_archived_room_records WHERE table_name='discord_ingress_journal'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&payload).unwrap(),
        before.payload
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&outcome).unwrap(),
        before.outcome.unwrap()
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&snapshot).unwrap()["confirmation_delivered"],
        0
    );
    assert!(
        connection
            .execute("DELETE FROM cdr_archived_room_records", [])
            .is_err()
    );
    assert!(
        connection
            .execute(
                "UPDATE cdr_archived_room_records SET snapshot_json='{}'",
                []
            )
            .is_err()
    );
}

#[tokio::test]
async fn active_or_missing_source_does_not_get_the_archived_exemption() {
    for active in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (sync, remote) = fixture(&temp);
        let db = temp.path().join("mirror.sqlite");
        rejection(&db, 801, &no_dispatch());
        let state = Connection::open(temp.path().join("state.sqlite")).unwrap();
        if active {
            // Restore the project registry as well as its source thread; the
            // base fixture intentionally omitted this archived project's row.
            upsert_project(&db, "C:/repos/old", "old", 21, 1.0, |a, b| a == b).unwrap();
            state
                .execute_batch("UPDATE threads SET archived=0 WHERE id='thread-old'")
                .unwrap();
        } else {
            state
                .execute_batch("DELETE FROM threads WHERE id='thread-old'")
                .unwrap();
        }
        let result = sync.sync(99, None).await.unwrap();
        assert_eq!(result.skipped.is_empty(), active);
        assert_preserved(&temp, &remote);
        assert_eq!(audit_count(&db), 0);
    }
}

#[path = "mirror_archive_completion.rs"]
mod completion;
