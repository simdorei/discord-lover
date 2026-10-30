use cdr_store::{async_question as aq, delivery_receipt, queue, schema::open_initialized};
use std::path::Path;

pub fn dispatching(db: &Path, runtime: &str) -> String {
    open_initialized(db)
        .unwrap()
        .execute(
            "INSERT INTO mirror_threads VALUES ('thread-b','project','title',10,20,0)",
            [],
        )
        .unwrap();
    queue::enqueue(
        db,
        queue::NewQueueJob {
            job_id: "origin",
            target_thread_id: "thread-b",
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
    queue::begin_attempt(db, "origin", &[], 1).unwrap();
    queue::mark_running(db, "origin", "original", 1).unwrap();
    let id = aq::observe(
        db,
        &aq::NewQuestion {
            runtime_id: runtime,
            generation: 1,
            thread_id: "thread-b",
            turn_id: "original",
            item_id: "question-call",
            body: &aq::QuestionBody {
                index: 0,
                source_text: "original context".into(),
                title: "Continue?".into(),
                options: vec!["yes".into(), "no".into()],
            },
            now: 2.0,
        },
    )
    .unwrap();
    let q = aq::get(db, &id).unwrap();
    let key = aq::receipt_key(&q).unwrap();
    delivery_receipt::begin(db, &key, "payload").unwrap();
    delivery_receipt::confirm(db, &key, "1234").unwrap();
    aq::bind_receipt(db, &id, true).unwrap();
    aq::begin_dispatch(
        db,
        &aq::Claim {
            id: &id,
            runtime_id: runtime,
            generation: 1,
            channel: 20,
            actor: 30,
            message: "1234",
            answer: aq::Answer::Choice(0),
            mode: aq::DispatchMode::Steer,
            baseline_turn_ids: Vec::new(),
            prompt: "only this answer",
            now: 3.0,
        },
    )
    .unwrap();
    id
}

pub fn pending(db: &Path, job: &str, thread: &str, generation: i64) {
    queue::enqueue(
        db,
        queue::NewQueueJob {
            job_id: job,
            target_thread_id: thread,
            channel_id: 20,
            owner_user_id: Some(30),
            discord_message_id: None,
            app_server_generation: generation,
            prompt: "new input",
            queued: true,
            ack_sent: true,
            created_at: 5.0,
        },
    )
    .unwrap();
}
