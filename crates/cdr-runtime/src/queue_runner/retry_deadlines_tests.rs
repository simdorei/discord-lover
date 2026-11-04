use super::*;
use std::sync::Arc;
#[path = "../../tests/support/queue_backoff.rs"]
mod support;

const ERROR: &str = "app-server returned error -32600 for thread/resume: thread target already has an active writer";

#[test]
fn stale_hints_cannot_authorize_a_changed_or_held_head() {
    for change in ["cancel", "head", "generation", "error", "starting"] {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("queue.sqlite");
        support::enqueue_job(&db, "head", "target", 1, 1.0);
        let connection = cdr_store::schema::open_initialized(&db).unwrap();
        connection
            .execute(
                "UPDATE codex_turn_queue SET attempt_count=9,last_error=?1,updated_at=1",
                [ERROR],
            )
            .unwrap();
        let queue = support::coordinator(&db, &Arc::new(support::BackoffBackend::default()));
        let job = cdr_store::queue::list(&db).unwrap().remove(0);
        queue.schedule_writer_retry(&job).unwrap();
        let hint = queue.due_writer_retries().unwrap().remove(0);
        match change {
            "cancel" | "head" => {
                connection
                    .execute("DELETE FROM codex_turn_queue", [])
                    .unwrap();
                if change == "head" {
                    support::enqueue_job(&db, "replacement", "target", 2, 2.0);
                }
            }
            "generation" => {
                connection
                    .execute("UPDATE codex_turn_queue SET app_server_generation=8", [])
                    .unwrap();
            }
            "error" => {
                connection
                    .execute(
                        "UPDATE codex_turn_queue SET last_error='transport timeout'",
                        [],
                    )
                    .unwrap();
            }
            "starting" => {
                cdr_store::queue::begin_attempt(&db, "head", &[], 7).unwrap();
            }
            _ => unreachable!(),
        }
        let lease = queue.try_target_lease("target").unwrap().unwrap();
        assert!(!lease.consume_writer_retry(&hint).unwrap(), "{change}");
        assert!(queue.due_writer_retries().unwrap().is_empty());
    }
}

#[test]
fn memory_gate_can_delay_a_hint_and_full_cache_remains_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("queue.sqlite");
    support::enqueue_job(&db, "head", "target", 1, 1.0);
    let connection = cdr_store::schema::open_initialized(&db).unwrap();
    connection
        .execute(
            "UPDATE codex_turn_queue SET attempt_count=9,last_error=?1,updated_at=1",
            [ERROR],
        )
        .unwrap();
    let queue = support::coordinator(&db, &Arc::new(support::BackoffBackend::default()));
    let mut job = cdr_store::queue::list(&db).unwrap().remove(0);
    queue
        .recovery_state
        .lock()
        .unwrap()
        .unavailable_logs
        .on_failure("target", "offline", Instant::now());
    queue.schedule_writer_retry(&job).unwrap();
    assert!(queue.due_writer_retries().unwrap().is_empty());
    for index in 0..256 {
        job.target_thread_id = format!("target-{index}");
        queue.schedule_writer_retry(&job).unwrap();
    }
    assert_eq!(queue.writer_retries.lock().unwrap().0.len(), HINT_CAP);
}
