use cdr_store::{
    archived_cleanup as cleanup,
    delivery_receipt::{self, ReceiptState},
    mapping, schema,
};
use rusqlite::{Connection, params};

fn prepare(path: &std::path::Path) -> cleanup::Intent {
    cleanup::prepare(
        path,
        &cleanup::Prepare {
            channel: 31,
            target: "old",
            parent: 21,
            guild: 1,
            origin: &serde_json::json!({"actor":7,"request":"sync-1"}),
            now: 20.0,
        },
        || Ok("{\"archived\":true}".into()),
    )
    .unwrap()
}

#[test]
fn eleven_unfinished_ingress_shapes_and_unknown_receipts_are_preserved_not_settled() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("mirror.sqlite");
    mapping::upsert_thread(&path, "old", "C:/repo", "old", 21, 31, 1.0).unwrap();
    let db = Connection::open(&path).unwrap();
    for i in 0..11 {
        let (state, phase, owner) = match i % 4 {
            0 => ("held", "processing", None),
            1 => ("held", "result_recorded", None),
            2 => ("owned", "handoff", Some("prompt")),
            _ => ("owned", "duplicate", Some("prompt")),
        };
        db.execute("INSERT INTO discord_ingress_journal(ingress_id,kind,channel_id,owner_user_id,payload_json,state,phase,target_thread_id,canonical_owner,owner_kind,owner_id,outcome_json,created_at,updated_at) VALUES(?,'message',31,7,?,? ,?,'old','canonical',?,?,'{\"unknown\":true}',1,1)",params![format!("m{i}"),format!("{{\"prompt\":\"original {i}\"}}"),state,phase,owner,owner.map(|_|format!("job-{i}"))]).unwrap();
    }
    db.execute("INSERT INTO codex_delivery_receipts(receipt_key,content_hash,retryable) VALUES('[31,\"final\",\"unknown\",0]','hash',0)",[]).unwrap();
    db.execute("INSERT INTO codex_delivery_receipts(receipt_key,content_hash,retryable) VALUES('[31,\"final\",\"retry\",0]','hash',1)",[]).unwrap();
    let before=db.query_row("SELECT json_group_array(json_array(ingress_id,state,phase,confirmation_delivered,payload_json,outcome_json,owner_id)) FROM discord_ingress_journal",[],|r|r.get::<_,String>(0)).unwrap();
    let permit = cleanup::effects::admit(&path, Some(31), None).unwrap();
    let intent = prepare(&path);
    assert_eq!(cleanup::effects::in_flight(&path, 31, "old").unwrap(), 1);
    assert!(cleanup::effects::admit(&path, Some(31), None).is_err());
    assert!(cleanup::effects::admit(&path, None, Some("old")).is_err());
    assert!(cleanup::effects::admit(&path, Some(99), Some("other")).is_ok());
    drop(permit);
    assert_eq!(cleanup::effects::in_flight(&path, 31, "old").unwrap(), 0);
    assert!(matches!(
        delivery_receipt::begin(&path, "[31,\"final\",\"retry\",0]", "hash").unwrap(),
        ReceiptState::Held(_)
    ));
    let sent = cleanup::transition(&path, &intent, "delete_attempted", "sent", 21.0).unwrap();
    let unknown =
        cleanup::transition(&path, &sent, "delete_unknown", "response lost", 22.0).unwrap();
    assert!(cleanup::transition(&path, &unknown, "delete_attempted", "retry", 23.0).is_err());
    let deleted = cleanup::transition(&path, &unknown, "deleted", "404", 24.0).unwrap();
    cleanup::retire_mapping(&path, &deleted, 25.0).unwrap();
    assert_eq!(mapping::thread_channels(&path, "old").unwrap(), None);
    assert_eq!(
        cleanup::counts(&path, &intent).unwrap()["discord_ingress_journal"],
        11
    );
    let after=db.query_row("SELECT json_group_array(json_array(ingress_id,state,phase,confirmation_delivered,payload_json,outcome_json,owner_id)) FROM discord_ingress_journal",[],|r|r.get::<_,String>(0)).unwrap();
    assert_eq!(before, after);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM cdr_live_discord_ingress_journal",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT retryable FROM codex_delivery_receipts WHERE receipt_key LIKE '%retry%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert!(
        db.execute("DELETE FROM cdr_archived_room_records", [])
            .is_err()
    );
    drop(schema::open_initialized(&path).unwrap());
    assert!(cleanup::effects::admit(&path, Some(31), None).is_err());
}

#[test]
fn failed_source_or_mapping_verification_creates_no_disposition() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("mirror.sqlite");
    mapping::upsert_thread(&path, "old", "C:/repo", "old", 21, 31, 1.0).unwrap();
    assert!(
        cleanup::prepare(
            &path,
            &cleanup::Prepare {
                channel: 31,
                target: "old",
                parent: 21,
                guild: 1,
                origin: &serde_json::Value::Null,
                now: 20.0
            },
            || Err(cdr_store::StoreError::Integrity("restored".into()))
        )
        .is_err()
    );
    assert!(cleanup::get(&path, 31).unwrap().is_none());
    assert!(cleanup::effects::admit(&path, Some(31), Some("old")).is_ok());
    assert_eq!(
        schema::schema_version(&Connection::open(&path).unwrap()).unwrap(),
        3
    );
}

#[test]
fn abort_then_rearchive_keeps_distinct_operations_and_never_revives_old_work() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db.sqlite");
    mapping::upsert_thread(&path, "old", "project", "old", 21, 31, 1.0).unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch("INSERT INTO discord_ingress_journal(ingress_id,kind,channel_id,owner_user_id,payload_json,state,phase,target_thread_id,created_at,updated_at) VALUES('old','message',31,7,'{}','staged','admitted','old',1,1)").unwrap();
    let first = prepare(&path);
    cleanup::transition(&path, &first, "aborted_source_changed", "restored", 21.0).unwrap();
    // SQLite REPLACE must not erase the original even after admission reopens.
    db.execute_batch("INSERT OR REPLACE INTO discord_ingress_journal(ingress_id,kind,channel_id,owner_user_id,payload_json,state,phase,target_thread_id,created_at,updated_at) VALUES('old','message',31,7,'{\"changed\":true}','done','confirmed','old',2,2)").unwrap();
    assert_eq!(
        db.query_row(
            "SELECT payload_json FROM discord_ingress_journal WHERE ingress_id='old'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "{}"
    );
    assert!(
        cleanup::effects::admit_record(
            &path,
            Some(31),
            None,
            Some(("discord_ingress_journal", "old"))
        )
        .is_err()
    );
    assert!(cleanup::effects::admit(&path, Some(31), Some("old")).is_ok());
    cdr_store::ingress::hold(&path, "old", "late worker ended", true, 22.0).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM codex_delivery_outbox", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(
        !cdr_store::ingress::begin_execution(&path, "old", "processing", Some("old"), 22.0)
            .unwrap()
    );
    let second = prepare(&path);
    assert_ne!(first.operation_id, second.operation_id);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM cdr_archived_room_dispositions",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    assert!(cleanup::effects::admit(&path, Some(31), Some("old")).is_err());
}

#[test]
fn legacy_fence_identity_change_cannot_retire_the_mapping() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db.sqlite");
    mapping::upsert_thread(&path, "old", "p", "old", 21, 31, 1.0).unwrap();
    cdr_store::room_cleanup::begin(&path, 31, Some("old"), 1.0).unwrap();
    let intent = prepare(&path);
    let deleted = cleanup::transition(&path, &intent, "deleted", "404", 21.0).unwrap();
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE cdr_cleanup_fences SET token='different-owner' WHERE channel_id=31",
            [],
        )
        .unwrap();
    assert!(cleanup::retire_mapping(&path, &deleted, 22.0).is_err());
    assert_eq!(
        mapping::thread_channels(&path, "old").unwrap(),
        Some((21, 31))
    );
    assert_eq!(cleanup::get(&path, 31).unwrap().unwrap().phase, "deleted");
}

#[test]
fn retired_request_inspection_requires_original_actor_and_recorded_parent() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db.sqlite");
    mapping::upsert_thread(&path, "old", "project", "old", 21, 31, 1.0).unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch("INSERT INTO discord_ingress_journal(ingress_id,kind,channel_id,owner_user_id,payload_json,state,phase,target_thread_id,created_at,updated_at) VALUES('old','message',31,7,'{\"text\":\"private original\"}','held','processing','old',1,1)").unwrap();
    let intent = prepare(&path);
    assert!(
        cdr_store::ingress::get_for_owner_readonly(&path, "old", 21, 7)
            .unwrap()
            .is_none()
    );
    let deleted = cleanup::transition(&path, &intent, "deleted", "404", 21.0).unwrap();
    cleanup::retire_mapping(&path, &deleted, 22.0).unwrap();
    assert!(
        cdr_store::ingress::get_for_owner_readonly(&path, "old", 21, 8)
            .unwrap()
            .is_none()
    );
    assert!(
        cdr_store::ingress::get_for_owner_readonly(&path, "old", 99, 7)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        cdr_store::ingress::get_for_owner_readonly(&path, "old", 21, 7)
            .unwrap()
            .unwrap()
            .payload["text"],
        "private original"
    );
    assert!(cleanup::inspection_parent(&path, 21).unwrap());
}

#[test]
fn conflicting_record_ownership_and_weakened_guards_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db.sqlite");
    mapping::upsert_thread(&path, "old", "project", "old", 21, 31, 1.0).unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch("INSERT INTO discord_ingress_journal(ingress_id,kind,channel_id,owner_user_id,payload_json,state,phase,target_thread_id,created_at,updated_at) VALUES('foreign','message',31,7,'{}','held','processing','foreign',1,1)").unwrap();
    assert!(
        cleanup::prepare(
            &path,
            &cleanup::Prepare {
                channel: 31,
                target: "old",
                parent: 21,
                guild: 1,
                origin: &serde_json::Value::Null,
                now: 20.0
            },
            || Ok("{}".into())
        )
        .is_err()
    );
    assert!(cleanup::get(&path, 31).unwrap().is_none());
    db.execute_batch("DROP VIEW cdr_live_discord_ingress_journal; CREATE VIEW cdr_live_discord_ingress_journal AS SELECT * FROM discord_ingress_journal;").unwrap();
    assert!(
        schema::open_initialized(&path)
            .unwrap_err()
            .to_string()
            .contains("malformed")
    );
}

#[test]
fn all_execution_families_are_retained_but_absent_from_live_work_and_pruning() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db.sqlite");
    mapping::upsert_thread(&path, "old", "project", "old", 21, 31, 1.0).unwrap();
    cdr_store::queue::enqueue(
        &path,
        cdr_store::queue::NewQueueJob {
            job_id: "queue",
            target_thread_id: "old",
            channel_id: 31,
            owner_user_id: Some(7),
            discord_message_id: None,
            app_server_generation: 1,
            prompt: "original",
            queued: true,
            ack_sent: false,
            created_at: 1.0,
        },
    )
    .unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch(r#"
        INSERT INTO codex_prompt_intakes(job_id,target_thread_id,channel_id,raw_prompt,auto_queue_when_busy,require_current_mirror,created_at,updated_at) VALUES('intake','old',31,'original intake',1,1,1,1);
        INSERT INTO codex_delivery_outbox(delivery_id,job_id,target_thread_id,turn_id,channel_id,content,created_at,updated_at) VALUES('final','queue','old','turn',31,'original final',1,1);
        INSERT INTO codex_commentary_outbox(delivery_key,job_id,target_thread_id,turn_id,channel_id,text) VALUES('progress','queue','old','turn',31,'original commentary');
        INSERT INTO codex_goal_progress(thread,turn,channel,content) VALUES('old','turn',31,'original goal');
        INSERT INTO cdr_async_questions(id,runtime_id,generation,thread_id,turn_id,item_id,origin_job_id,channel_id,owner_user_id,body,created_at,updated_at) VALUES('question','runtime',1,'old','turn','item','queue',31,7,'{"index":0,"title":"original question","options":["yes"]}',1,1);
        INSERT INTO cdr_idle_release(intent_id,owner_id,generation,thread_id,turn_id,job_id,revision,state) VALUES('idle','owner',1,'old','turn','queue',1,'Unknown');
        INSERT INTO codex_dead_generation_holds VALUES('old','runtime',1,1);
        INSERT INTO codex_mutation_attempts(attempt_id,runtime_id,owner_id,generation,wire_id,method,target_thread_id,scoped,request_sha256,state,created_at,updated_at) VALUES('mutation','runtime','owner',1,'wire','turn/start','old',1,'original hash','prepared',1,1);
        INSERT INTO cdr_stop_controls(operation_id,target_thread_id,resident_owner,generation,turn_id,record_json,phase) VALUES('stop','old','owner',1,'turn','{}','unknown');
        INSERT INTO cdr_server_responses VALUES('response','runtime','owner',1,'old','turn','queue','{}','hash','admitted',1,1,NULL);
        INSERT INTO codex_delivery_receipts(receipt_key,content_hash) VALUES('[31,"final","final",0]','original hash');
    "#).unwrap();
    let intent = prepare(&path);
    let tables = [
        "codex_turn_queue",
        "codex_prompt_intakes",
        "codex_delivery_outbox",
        "codex_commentary_outbox",
        "codex_goal_progress",
        "cdr_async_questions",
        "cdr_idle_release",
        "codex_dead_generation_holds",
        "codex_mutation_attempts",
        "cdr_stop_controls",
        "cdr_server_responses",
        "codex_delivery_receipts",
    ];
    for table in tables {
        assert_eq!(
            cleanup::counts(&path, &intent).unwrap()[table],
            1,
            "{table}"
        );
        assert_eq!(
            db.query_row(&format!("SELECT count(*) FROM cdr_live_{table}"), [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "{table}"
        );
    }
    assert!(
        cdr_store::prompt_intake::try_claim_prompt_intake(&path, "intake", 21.0, 30.0)
            .unwrap()
            .is_none()
    );
    let deleted = cleanup::transition(&path, &intent, "deleted", "404", 21.0).unwrap();
    cleanup::retire_mapping(&path, &deleted, 22.0).unwrap();
    cdr_store::async_question::retire_old_owner(&path, "other", 2).unwrap();
    cdr_store::async_question::compact_terminal(&path, 1e10).unwrap();
    for table in tables {
        db.execute(&format!("DELETE FROM {table}"), []).unwrap();
        assert_eq!(
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1,
            "{table}"
        );
    }
    mapping::upsert_thread(&path, "old", "project", "restored", 21, 32, 30.0).unwrap();
    assert!(cleanup::effects::admit(&path, Some(32), Some("old")).is_ok());
    assert!(matches!(
        delivery_receipt::begin(&path, "[32,\"completion/v1\",\"final\",0]", "hash").unwrap(),
        ReceiptState::Held(_)
    ));
    assert!(matches!(
        delivery_receipt::begin(&path, "[32,\"async-question-v1\",\"question\",0]", "hash")
            .unwrap(),
        ReceiptState::Held(_)
    ));
    assert!(cdr_store::queue::list(&path).unwrap().is_empty());
    assert_eq!(
        cdr_store::async_question::get(&path, "question")
            .unwrap()
            .state,
        "observed"
    );
}
