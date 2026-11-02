#[path = "support/legacy_schema.rs"]
mod legacy_schema;
use cdr_store::{
    delivery, idle_release as idle, mapping, queue, reserve_policy, room_cleanup,
    schema::open_initialized,
};
use std::path::Path;

fn candidate(db: &Path) -> idle::Intent {
    mapping::upsert_thread(db, "thread", "p", "title", 100, 42, 1.0).unwrap();
    queue::enqueue(
        db,
        queue::NewQueueJob {
            job_id: "job",
            target_thread_id: "thread",
            channel_id: 42,
            owner_user_id: Some(3),
            discord_message_id: None,
            app_server_generation: 1,
            prompt: "input",
            queued: false,
            ack_sent: true,
            created_at: 1.0,
        },
    )
    .unwrap();
    queue::begin_attempt(db, "job", &[], 1).unwrap();
    queue::mark_running(db, "job", "turn", 1).unwrap();
    let out = delivery::stage_queue_completion_with_release(
        db,
        "job",
        "Final",
        2.0,
        Some(("resident", 1)),
    )
    .unwrap();
    delivery::complete(db, &out.delivery_id).unwrap();
    idle::get(db, "thread").unwrap().unwrap()
}

#[test]
fn cleanup_cancels_only_unsent_candidate_with_its_fence_transaction() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("db.sqlite");
    let old = candidate(&db);
    assert_eq!(
        room_cleanup::pending_reason(&db, 42, Some("thread")).unwrap(),
        None
    );
    room_cleanup::begin(&db, 42, Some("thread"), 3.0).unwrap();
    assert_eq!(idle::get(&db, "thread").unwrap().unwrap().state, "Settled");
    assert!(idle::transition(&db, &old, "Dispatching", "late worker").is_err());
    assert!(!idle::bot_idle(&db, "thread").unwrap());
}

#[test]
fn failed_cleanup_does_not_consume_candidate_or_unanswered_question() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("db.sqlite");
    let old = candidate(&db);
    open_initialized(&db).unwrap().execute_batch("INSERT INTO cdr_async_questions(id,runtime_id,generation,thread_id,turn_id,item_id,origin_job_id,channel_id,owner_user_id,body,state,created_at,updated_at) VALUES('q','resident',1,'thread','turn','item','job',42,3,'{}','open',1,1);").unwrap();
    assert!(room_cleanup::begin(&db, 42, Some("thread"), 3.0).is_err());
    assert_eq!(idle::get(&db, "thread").unwrap().unwrap(), old);
    assert!(room_cleanup::phase(&db, 42).unwrap().is_none());
}

#[test]
fn unresolved_idle_states_block_all_cleanup_preflights_and_begin() {
    for state in ["Dispatching", "AwaitUnload", "Resubscribing", "Unknown"] {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("db.sqlite");
        candidate(&db);
        open_initialized(&db)
            .unwrap()
            .execute("UPDATE cdr_idle_release SET state=?", [state])
            .unwrap();
        for reason in [
            room_cleanup::pending_reason(&db, 42, Some("thread")),
            room_cleanup::pending_reason_pre_commentary_schema(&db, 42, Some("thread")),
        ] {
            assert_eq!(
                reason.unwrap(),
                Some("unsettled idle subscription release"),
                "{state}"
            );
        }
        assert!(room_cleanup::begin(&db, 42, Some("thread"), 3.0).is_err());
        assert_eq!(idle::get(&db, "thread").unwrap().unwrap().state, state);
    }
}

#[test]
fn old_feature_shapes_migrate_without_discarding_existing_work() {
    for source in ["reserve-parent", "async-idle-parent"] {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("db.sqlite");
        candidate(&db);
        reserve_policy::ensure(&db, "thread").unwrap();
        let connection = open_initialized(&db).unwrap();
        legacy_schema::remove_retirement_guards(&connection);
        // These guards did not exist in either historical parent. Remove their
        // dependencies before manufacturing the old column/table shape, then
        // require migration to recreate every guard below.
        connection
            .execute_batch(
                "DROP TRIGGER cdr_async_obligation_question;
             DROP TRIGGER cdr_async_obligation_queue_delete;
             DROP TRIGGER cdr_async_source_seal_immutable;
             DROP TRIGGER cdr_async_uncopied_origin_guard;
             DROP TRIGGER cdr_async_obligation_attempt;",
            )
            .unwrap();
        connection.execute_batch("CREATE TABLE foreign_sentinel(value TEXT); INSERT INTO foreign_sentinel VALUES('keep'); ALTER TABLE codex_observed_completions DROP COLUMN resident_owner; ALTER TABLE cdr_async_questions DROP COLUMN preparation_json;").unwrap();
        if source == "reserve-parent" {
            connection.execute_batch("DROP TABLE cdr_async_questions; DROP TABLE cdr_async_question_inbox; DROP TABLE cdr_idle_release;").unwrap();
        } else {
            connection.execute_batch("DROP TABLE codex_reserve_transition_notices; DROP TABLE codex_reserve_start_notices; DROP TABLE codex_reserve_policy; DROP TABLE cdr_archived_cleanup_evidence;").unwrap();
        }
        drop(connection);
        let upgraded = open_initialized(&db).unwrap();
        let guards: i64 = upgraded
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND name IN (
             'cdr_async_obligation_question','cdr_async_obligation_queue_delete',
             'cdr_async_source_seal_immutable','cdr_async_uncopied_origin_guard',
             'cdr_async_obligation_attempt')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            guards, 5,
            "{source}: migration must restore every safety guard"
        );
        let count:i64=upgraded.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name IN ('cdr_idle_release','cdr_async_questions','cdr_async_question_inbox','codex_reserve_policy','codex_reserve_start_notices','codex_reserve_transition_notices','cdr_archived_cleanup_evidence')",[],|r|r.get(0)).unwrap();
        assert_eq!(count, 7, "{source}");
        assert_eq!(
            upgraded
                .query_row("SELECT value FROM foreign_sentinel", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "keep"
        );
        assert_eq!(upgraded.query_row("SELECT COUNT(*) FROM mirror_threads WHERE codex_thread_id='thread' AND discord_thread_id=42", [], |r| r.get::<_,i64>(0)).unwrap(), 1);
        if source == "async-idle-parent" {
            assert_eq!(
                idle::get(&db, "thread").unwrap().unwrap().state,
                "Candidate"
            );
        } else {
            assert_eq!(
                reserve_policy::get(&db, "thread").unwrap().unwrap().state,
                "ordinary"
            );
        }
    }
}
