//! Migration keeps legacy pending data but cannot invent cross-generation proof.
use cdr_store::{async_question as aq, queue, schema::open_initialized};
#[path = "support/legacy_schema.rs"]
mod legacy_schema;

#[test]
fn legacy_candidate_keeps_original_generation_only_and_never_inherits_by_migration() {
    for observed in [1, 2] {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("store.sqlite");
        queue::enqueue(
            &db,
            queue::NewQueueJob {
                job_id: "job",
                target_thread_id: "thread",
                channel_id: 20,
                owner_user_id: Some(30),
                discord_message_id: None,
                app_server_generation: 1,
                prompt: "old",
                queued: false,
                ack_sent: true,
                created_at: 1.0,
            },
        )
        .unwrap();
        queue::begin_attempt(&db, "job", &[], 1).unwrap();
        queue::mark_running(&db, "job", "T1", 1).unwrap();
        aq::record_observation(
            &db,
            &aq::NewQuestion {
                runtime_id: "runtime",
                generation: observed,
                thread_id: "thread",
                turn_id: "T2",
                item_id: "item",
                body: &aq::QuestionBody {
                    index: 0,
                    source_text: String::new(),
                    title: "?".into(),
                    options: vec![],
                },
                now: 2.0,
            },
        )
        .unwrap();
        let id = aq::occurrence_id("thread", "T2", "item", 0).unwrap();
        let legacy = open_initialized(&db).unwrap();
        legacy_schema::remove_retirement_guards(&legacy);
        legacy
            .execute_batch(
                "ALTER TABLE cdr_async_question_inbox DROP COLUMN candidate_generation;
             ALTER TABLE cdr_async_question_inbox DROP COLUMN candidate_execution_generation;
             ALTER TABLE cdr_async_question_inbox DROP COLUMN candidate_attempt_count;",
            )
            .unwrap();
        let conn = open_initialized(&db).unwrap();
        let absent:bool=conn.query_row(
            "SELECT candidate_generation IS NULL AND candidate_execution_generation IS NULL AND candidate_attempt_count IS NULL FROM cdr_async_question_inbox WHERE id=?",[&id],|r|r.get(0),
        ).unwrap();
        assert!(absent);
        drop(conn);
        queue::mark_goal_waiting(&db, "job", "T1", 1).unwrap();
        let before = queue::list(&db).unwrap().remove(0);
        queue::attach_goal_turn_observed_if_owned(&db, &before, "T2", observed).unwrap();
        assert_eq!(
            aq::reconcile_observations(&db, "runtime", observed).unwrap(),
            usize::from(observed == 1)
        );
        assert_eq!(aq::get(&db, &id).is_ok(), observed == 1);
        assert_eq!(queue::list(&db).unwrap()[0].app_server_generation, 1);
    }
}

#[test]
fn invalid_observation_identity_never_creates_a_candidate() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("store.sqlite");
    for (generation, thread, turn, item) in [
        (-1, "thread", "turn", "item"),
        (1, " ", "turn", "item"),
        (1, "thread", " ", "item"),
        (1, "thread", "turn", " "),
    ] {
        let body = aq::QuestionBody {
            index: 0,
            source_text: String::new(),
            title: "?".into(),
            options: vec![],
        };
        let error = aq::record_observation(
            &db,
            &aq::NewQuestion {
                runtime_id: "runtime",
                generation,
                thread_id: thread,
                turn_id: turn,
                item_id: item,
                body: &body,
                now: 1.0,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid or oversized"));
    }
}
