//! Revision 13: original execution and current-turn observation are separate evidence.
#[path = "support/legacy_schema.rs"]
mod legacy_schema;
use cdr_store::{observed_completion, observed_final_answer, queue};
use std::path::Path;

fn waiting(db: &Path) -> queue::StoredQueueJob {
    queue::enqueue(
        db,
        queue::NewQueueJob {
            job_id: "job",
            target_thread_id: "thread",
            channel_id: 42,
            owner_user_id: Some(1),
            discord_message_id: None,
            app_server_generation: 1,
            prompt: "original",
            queued: false,
            ack_sent: true,
            created_at: 1.0,
        },
    )
    .unwrap();
    queue::begin_attempt(db, "job", &["baseline".into()], 1).unwrap();
    queue::mark_running(db, "job", "T1", 1).unwrap();
    queue::mark_goal_waiting(db, "job", "T1", 1).unwrap();
    queue::list(db).unwrap().remove(0)
}

#[test]
fn attached_observation_generation_controls_both_journals_not_original_execution() {
    for legacy in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("db.sqlite");
        waiting(&db);
        if legacy {
            rusqlite::Connection::open(&db)
                .unwrap()
                .execute("UPDATE codex_turn_queue SET execution_generation=NULL", [])
                .unwrap();
        }
        let before = queue::list(&db).unwrap().remove(0);
        assert!(queue::attach_goal_turn_observed_if_owned(&db, &before, "T2", 2).unwrap());
        queue::adopt_generation(&db, 9).unwrap();
        let after = queue::list(&db).unwrap().remove(0);
        assert_eq!(after.app_server_generation, before.app_server_generation);
        assert_eq!(after.execution_generation, before.execution_generation);
        assert_eq!(after.turn_observation_generation, Some(2));
        assert_eq!(after.completion_evidence_generation(), 2);
        assert_eq!(after.attempt_count, before.attempt_count);
        assert_eq!(after.baseline_turn_ids, before.baseline_turn_ids);
        for (thread, turn, generation) in [
            ("thread", "T2", 1),
            ("thread", "T1", 1),
            ("foreign", "T2", 2),
            ("thread", "foreign", 2),
        ] {
            assert!(!observed_completion::record(&db, thread, turn, generation, "{}").unwrap());
            assert!(
                !observed_final_answer::record(&db, thread, turn, generation, "foreign").unwrap()
            );
        }
        assert!(observed_completion::record(&db, "thread", "T2", 2, "terminal").unwrap());
        assert!(observed_final_answer::record(&db, "thread", "T2", 2, "exact").unwrap());
        assert!(!observed_final_answer::record(&db, "thread", "T2", 2, "changed").unwrap());
        assert_eq!(
            observed_completion::pending_with_generation(&db).unwrap(),
            [("thread".into(), "T2".into(), 2, "terminal".into())]
        );
        assert_eq!(
            observed_final_answer::get(&db, "thread", "T2", 2).unwrap(),
            Some("exact".into())
        );
    }
}

#[test]
fn observation_field_is_part_of_exact_owner_cas_and_negative_generation_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("db.sqlite");
    let before = waiting(&db);
    let mut stale = before.clone();
    stale.turn_observation_generation = Some(99);
    assert!(!queue::attach_goal_turn_observed_if_owned(&db, &stale, "T2", 2).unwrap());
    assert!(!queue::attach_goal_turn_observed_if_owned(&db, &before, "T2", -1).unwrap());
    assert_eq!(queue::list(&db).unwrap(), [before]);
}

#[test]
fn failed_turn_attachment_cannot_commit_only_the_observation_generation() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("db.sqlite");
    let before = waiting(&db);
    rusqlite::Connection::open(&db).unwrap().execute_batch(
        "CREATE TRIGGER fail_observation BEFORE UPDATE OF turn_observation_generation ON codex_turn_queue
         BEGIN SELECT RAISE(ABORT,'observation write failed'); END;",
    ).unwrap();
    let error = queue::attach_goal_turn_observed_if_owned(&db, &before, "T2", 2).unwrap_err();
    assert!(error.to_string().contains("observation write failed"));
    assert_eq!(queue::list(&db).unwrap(), [before]);
    assert!(!observed_completion::record(&db, "thread", "T2", 2, "{}").unwrap());
}

#[test]
fn old_schema_migrates_without_fabricating_turn_or_execution_generation() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("db.sqlite");
    waiting(&db);
    let connection = rusqlite::Connection::open(&db).unwrap();
    legacy_schema::remove_retirement_guards(&connection);
    connection
        .execute_batch(
            // These modern capture triggers do not exist in the legacy fixture.
            "DROP TRIGGER cdr_async_obligation_question;
         DROP TRIGGER cdr_async_obligation_queue_delete;
         UPDATE codex_turn_queue SET execution_generation=NULL;
        ALTER TABLE codex_turn_queue DROP COLUMN turn_observation_generation;",
        )
        .unwrap();
    drop(connection);
    let inherited = queue::list(&db).unwrap().remove(0);
    assert_eq!(inherited.turn_observation_generation, None);
    assert_eq!(inherited.execution_generation, None);
    assert_eq!(inherited.app_server_generation, 1);
    assert_eq!(inherited.completion_evidence_generation(), 1);
    assert!(inherited.goal_waiting);
    assert_eq!(inherited.state, queue::QueueJobState::Running);
    assert_eq!(inherited.prompt, "original");
    assert_eq!(inherited.turn_id.as_deref(), Some("T1"));
    assert_eq!(inherited.attempt_count, 1);
    assert_eq!(inherited.baseline_turn_ids, ["baseline"]);
    assert!(queue::attach_goal_turn_observed_if_owned(&db, &inherited, "T2", 2).unwrap());
    assert_eq!(
        queue::list(&db).unwrap()[0].turn_observation_generation,
        Some(2)
    );
}

#[test]
fn a_new_attempt_does_not_reuse_an_old_goal_turn_observation() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("db.sqlite");
    let before = waiting(&db);
    assert!(queue::attach_goal_turn_observed_if_owned(&db, &before, "T2", 2).unwrap());
    let starting = queue::begin_attempt(&db, "job", &["T1".into(), "T2".into()], 1).unwrap();
    assert_eq!(starting.turn_observation_generation, None);
    assert_eq!(starting.turn_id, None);
    let running = queue::mark_running(&db, "job", "T3", 1).unwrap();
    assert_eq!(running.turn_observation_generation, Some(1));
    assert!(!observed_final_answer::record(&db, "thread", "T3", 2, "old").unwrap());
    assert!(observed_final_answer::record(&db, "thread", "T3", 1, "new").unwrap());
}
