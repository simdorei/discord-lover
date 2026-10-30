//! R1: immutable execution provenance is not current question/turn authority.
use cdr_store::{
    async_question as aq, delivery, delivery_receipt, queue, schema::open_initialized,
};
use rusqlite::params;
use std::path::{Path, PathBuf};

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.sqlite");
    open_initialized(&db)
        .unwrap()
        .execute(
            "INSERT INTO mirror_threads VALUES ('thread','project','title',10,20,0)",
            [],
        )
        .unwrap();
    queue::enqueue(
        &db,
        queue::NewQueueJob {
            job_id: "origin",
            target_thread_id: "thread",
            channel_id: 20,
            owner_user_id: Some(30),
            discord_message_id: Some(40),
            app_server_generation: 1,
            prompt: "original input",
            queued: false,
            ack_sent: true,
            created_at: 1.0,
        },
    )
    .unwrap();
    queue::begin_attempt(&db, "origin", &[], 1).unwrap();
    queue::mark_running(&db, "origin", "T1", 1).unwrap();
    (dir, db)
}

fn journal(db: &Path, generation: i64) -> String {
    aq::record_observation(
        db,
        &aq::NewQuestion {
            runtime_id: "current",
            generation,
            thread_id: "thread",
            turn_id: "T2",
            item_id: "item",
            body: &aq::QuestionBody {
                index: 0,
                source_text: String::new(),
                title: "Continue?".into(),
                options: vec!["yes".into(), "no".into()],
            },
            now: 2.0,
        },
    )
    .unwrap();
    aq::occurrence_id("thread", "T2", "item", 0).unwrap()
}

fn attach(db: &Path) {
    assert!(queue::mark_goal_waiting(db, "origin", "T1", 1).unwrap());
    let before = queue::list(db).unwrap().remove(0);
    assert!(queue::attach_goal_turn_observed_if_owned(db, &before, "T2", 2).unwrap());
    let after = queue::list(db).unwrap().remove(0);
    assert_eq!(after.app_server_generation, before.app_server_generation);
    assert_eq!(after.execution_generation, before.execution_generation);
    assert_eq!(after.attempt_count, before.attempt_count);
    assert_eq!(after.turn_observation_generation, Some(2));
}

fn bind(db: &Path, id: &str) {
    let q = aq::get(db, id).unwrap();
    let key = aq::receipt_key(&q).unwrap();
    delivery_receipt::begin(db, &key, "payload").unwrap();
    delivery_receipt::confirm(db, &key, "1234").unwrap();
    aq::bind_receipt(db, id, true).unwrap();
}

fn claim(id: &str) -> aq::Claim<'_> {
    aq::Claim {
        id,
        runtime_id: "current",
        generation: 2,
        channel: 20,
        actor: 30,
        message: "1234",
        answer: aq::Answer::Choice(0),
        mode: aq::DispatchMode::Steer,
        baseline_turn_ids: Vec::new(),
        prompt: "only this answer",
        now: 3.0,
    }
}

#[test]
fn inherited_current_question_binds_claims_and_confirms_exact_steer_once() {
    let (_dir, db) = fixture();
    attach(&db);
    let id = journal(&db, 2);
    assert_eq!(aq::reconcile_observations(&db, "current", 2).unwrap(), 1);
    let q = aq::get(&db, &id).unwrap();
    assert_eq!(q.generation, 2);
    assert_eq!(q.origin_job_id, "origin");
    open_initialized(&db)
        .unwrap()
        .execute(
            "UPDATE cdr_async_questions SET owner_confirmed=0 WHERE id=?",
            [&id],
        )
        .unwrap();
    assert!(aq::owner_confirmed(&db, &q).unwrap());
    bind(&db, &id);
    let before = queue::list(&db).unwrap();
    aq::begin_dispatch(&db, &claim(&id)).unwrap();
    aq::validate_dispatch_guards(&db, "thread").unwrap();
    assert!(aq::confirm_dispatch(&db, &id, "T1").is_err());
    aq::confirm_dispatch(&db, &id, "T2").unwrap();
    assert_eq!(aq::get(&db, &id).unwrap().state, "submitted");
    assert_eq!(queue::list(&db).unwrap(), before);
    assert!(aq::begin_dispatch(&db, &claim(&id)).is_err());
}

#[test]
fn early_current_question_is_data_only_until_exact_inherited_handoff() {
    let (_dir, db) = fixture();
    let id = journal(&db, 2);
    assert_eq!(aq::reconcile_observations(&db, "current", 2).unwrap(), 0);
    assert!(aq::get(&db, &id).is_err());
    assert_eq!(
        aq::reconcile_observations(&db, "other-runtime", 2).unwrap(),
        0
    );
    attach(&db);
    assert_eq!(aq::reconcile_observations(&db, "current", 2).unwrap(), 1);
    assert_eq!(journal(&db, 2), id);
    assert_eq!(aq::reconcile_observations(&db, "current", 2).unwrap(), 0);
}

#[test]
fn old_observation_generation_cannot_authorize_current_goal_turn() {
    let (_dir, db) = fixture();
    attach(&db);
    let id = journal(&db, 1);
    assert_eq!(aq::reconcile_observations(&db, "current", 1).unwrap(), 0);
    assert!(aq::get(&db, &id).is_err());
}

#[test]
fn current_candidate_cannot_follow_mutated_execution_or_actor_identity() {
    for sql in [
        "UPDATE codex_turn_queue SET app_server_generation=9",
        "UPDATE codex_turn_queue SET execution_generation=9",
        "UPDATE codex_turn_queue SET attempt_count=99",
        "UPDATE codex_turn_queue SET owner_user_id=31",
        "UPDATE codex_turn_queue SET channel_id=21",
        "UPDATE codex_turn_queue SET turn_id='wrong'",
        "UPDATE codex_turn_queue SET goal_waiting=1",
    ] {
        let (_dir, db) = fixture();
        let id = journal(&db, 2);
        attach(&db);
        open_initialized(&db).unwrap().execute(sql, []).unwrap();
        assert_eq!(
            aq::reconcile_observations(&db, "current", 2).unwrap(),
            0,
            "{sql}"
        );
        assert!(aq::get(&db, &id).is_err(), "{sql}");
    }
}

#[test]
fn retired_current_candidate_is_never_revived_or_rebound() {
    let (_dir, db) = fixture();
    let id = journal(&db, 2);
    aq::retire_old_owner(&db, "other", 2).unwrap();
    attach(&db);
    assert_eq!(journal(&db, 2), id);
    assert_eq!(aq::reconcile_observations(&db, "current", 2).unwrap(), 0);
    assert!(aq::get(&db, &id).is_err());
}

#[test]
fn another_generation_owner_is_not_hidden_during_candidate_selection_or_claim() {
    for after_promotion in [false, true] {
        let (_dir, db) = fixture();
        attach(&db);
        let id = if after_promotion {
            let id = journal(&db, 2);
            aq::reconcile_observations(&db, "current", 2).unwrap();
            bind(&db, &id);
            id
        } else {
            String::new()
        };
        queue::enqueue(
            &db,
            queue::NewQueueJob {
                job_id: "duplicate",
                target_thread_id: "thread",
                channel_id: 20,
                owner_user_id: Some(30),
                discord_message_id: None,
                app_server_generation: 9,
                prompt: "other",
                queued: true,
                ack_sent: true,
                created_at: 4.0,
            },
        )
        .unwrap();
        open_initialized(&db).unwrap().execute(
            "UPDATE codex_turn_queue SET state='running',turn_id='other' WHERE job_id='duplicate'", [],
        ).unwrap();
        if after_promotion {
            assert!(aq::begin_dispatch(&db, &claim(&id)).is_err());
        } else {
            let body = aq::QuestionBody {
                index: 0,
                source_text: String::new(),
                title: "?".into(),
                options: vec![],
            };
            assert!(
                aq::record_observation(
                    &db,
                    &aq::NewQuestion {
                        runtime_id: "current",
                        generation: 2,
                        thread_id: "thread",
                        turn_id: "T2",
                        item_id: "item",
                        body: &body,
                        now: 4.0,
                    }
                )
                .is_err()
            );
        }
    }
}

#[test]
fn completion_before_reconciliation_preserves_exact_current_question_ownership() {
    let (_dir, db) = fixture();
    let id = journal(&db, 2);
    attach(&db);
    let expected = queue::list(&db).unwrap().remove(0);
    delivery::stage_owned_queue_completion_with_release(
        &db,
        &expected,
        "Final",
        4.0,
        Some(("current", 2)),
    )
    .unwrap();
    assert!(queue::list(&db).unwrap().is_empty());
    let q = aq::get(&db, &id).unwrap();
    assert!(aq::owner_confirmed(&db, &q).unwrap());
    assert_eq!(q.generation, 2);
    assert_eq!(q.origin_job_id, "origin");
    assert_eq!(delivery::list_pending(&db).unwrap().len(), 1);
    bind(&db, &id);
    let mut c = claim(&id);
    c.mode = aq::DispatchMode::Start;
    c.baseline_turn_ids = vec!["T1".into(), "T2".into()];
    aq::begin_dispatch(&db, &c).unwrap();
    aq::validate_dispatch_guards(&db, "thread").unwrap();
    aq::confirm_dispatch(&db, &id, "answer-turn").unwrap();
    assert_eq!(queue::list(&db).unwrap()[0].app_server_generation, 2);
}

#[test]
fn generationless_outbox_does_not_promote_stale_or_unbound_questions() {
    let (_dir, db) = fixture();
    let id = journal(&db, 1);
    attach(&db);
    let expected = queue::list(&db).unwrap().remove(0);
    delivery::stage_owned_queue_completion_with_release(&db, &expected, "Final", 4.0, None)
        .unwrap();
    assert_eq!(aq::reconcile_observations(&db, "current", 1).unwrap(), 0);
    assert!(aq::get(&db, &id).is_err());
}

#[test]
fn failed_question_ownership_commit_rolls_back_final_and_job_deletion() {
    let (_dir, db) = fixture();
    let id = journal(&db, 2);
    attach(&db);
    let before = queue::list(&db).unwrap().remove(0);
    open_initialized(&db).unwrap().execute_batch(
        "CREATE TRIGGER fail_question BEFORE INSERT ON cdr_async_questions BEGIN SELECT RAISE(ABORT,'question persistence failed'); END;",
    ).unwrap();
    assert!(
        delivery::stage_owned_queue_completion_with_release(&db, &before, "Final", 4.0, None)
            .is_err()
    );
    assert_eq!(queue::list(&db).unwrap(), [before]);
    assert!(delivery::list_pending(&db).unwrap().is_empty());
    assert!(aq::get(&db, &id).is_err());
    let count: i64 = open_initialized(&db)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM cdr_async_question_inbox WHERE id=?",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn post_claim_observation_change_cannot_pass_actual_write_or_confirmation_guard() {
    let (_dir, db) = fixture();
    attach(&db);
    let id = journal(&db, 2);
    aq::reconcile_observations(&db, "current", 2).unwrap();
    bind(&db, &id);
    aq::begin_dispatch(&db, &claim(&id)).unwrap();
    open_initialized(&db)
        .unwrap()
        .execute(
            "UPDATE codex_turn_queue SET turn_observation_generation=?",
            params![3],
        )
        .unwrap();
    assert!(aq::validate_dispatch_guards(&db, "thread").is_err());
    assert!(aq::confirm_dispatch(&db, &id, "T2").is_err());
    assert!(aq::reject_definite(&db, &id, "late rejection").is_err());
    assert_eq!(aq::get(&db, &id).unwrap().state, "dispatching");
}
