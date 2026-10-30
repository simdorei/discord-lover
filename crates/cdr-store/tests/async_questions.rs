use cdr_store::{async_question as aq, delivery_receipt, queue, schema::open_initialized};
use rusqlite::params;

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("store.sqlite");
    let conn = open_initialized(&db).unwrap();
    conn.execute(
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
            prompt: "prompt",
            queued: false,
            ack_sent: true,
            created_at: 0.0,
        },
    )
    .unwrap();
    conn.execute(
        "UPDATE codex_turn_queue SET state='running',turn_id='original'",
        [],
    )
    .unwrap();
    let body = aq::QuestionBody {
        index: 1,
        source_text: String::new(),
        title: "프로젝트 B?".into(),
        options: vec!["허용".into(), "보류".into()],
    };
    let id = aq::observe(
        &db,
        &aq::NewQuestion {
            runtime_id: "resident-a",
            generation: 1,
            thread_id: "thread",
            turn_id: "original",
            item_id: "call-id",
            body: &body,
            now: 1.0,
        },
    )
    .unwrap();
    let key = aq::receipt_key(&aq::get(&db, &id).unwrap()).unwrap();
    delivery_receipt::begin(&db, &key, "payload-hash").unwrap();
    delivery_receipt::confirm(&db, &key, "1234").unwrap();
    aq::bind_receipt(&db, &id, true).unwrap();
    (dir, db, id)
}

fn claim(id: &str, mode: aq::DispatchMode) -> aq::Claim<'_> {
    aq::Claim {
        id,
        runtime_id: "resident-a",
        generation: 1,
        channel: 20,
        actor: 30,
        message: "1234",
        answer: aq::Answer::Choice(1),
        mode,
        baseline_turn_ids: if mode == aq::DispatchMode::Start {
            vec!["older-ui-turn".into(), "original".into()]
        } else {
            Vec::new()
        },
        prompt: "B=보류",
        now: 2.0,
    }
}

#[test]
fn competing_buttons_are_one_durable_claim_without_ttl_reopening() {
    let (_dir, db, id) = fixture();
    let first = aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Steer)).unwrap();
    assert_eq!(first.chosen, Some(1));
    let mut competing = claim(&id, aq::DispatchMode::Steer);
    competing.answer = aq::Answer::Choice(0);
    competing.now = 9_999_999.0;
    assert!(aq::begin_dispatch(&db, &competing).is_err());
    assert_eq!(aq::get(&db, &id).unwrap().chosen, Some(1));
    assert_eq!(aq::get(&db, &id).unwrap().state, "dispatching");
}

#[test]
fn completed_reply_is_quarantined_until_exact_acceptance_commits() {
    let (_dir, db, id) = fixture();
    queue::complete(&db, "origin").unwrap();
    let dispatch = aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).unwrap();
    let jobs = queue::list(&db).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, queue::QueueJobState::Quarantined);
    assert_eq!(jobs[0].baseline_turn_ids, ["older-ui-turn", "original"]);
    assert_eq!(
        dispatch.reply_job_id.as_deref(),
        Some(jobs[0].job_id.as_str())
    );
    aq::confirm_dispatch(&db, &id, "accepted-successor").unwrap();
    let jobs = queue::list(&db).unwrap();
    assert_eq!(jobs[0].state, queue::QueueJobState::Running);
    assert_eq!(jobs[0].turn_id.as_deref(), Some("accepted-successor"));
    assert_eq!(jobs[0].baseline_turn_ids, ["older-ui-turn", "original"]);
    assert_eq!(aq::get(&db, &id).unwrap().state, "submitted");
    assert!(aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).is_err());
}

#[test]
fn forged_or_wrong_authority_never_claims_and_changed_mapping_rejects() {
    let (_dir, db, id) = fixture();
    for change in 0..6 {
        let mut c = claim(&id, aq::DispatchMode::Steer);
        match change {
            0 => c.actor = 31,
            1 => c.channel = 21,
            2 => c.message = "999",
            3 => c.answer = aq::Answer::Choice(10),
            4 => c.runtime_id = "resident-b",
            _ => c.generation = 2,
        }
        assert!(aq::begin_dispatch(&db, &c).is_err());
        assert_eq!(aq::get(&db, &id).unwrap().state, "open");
    }
    open_initialized(&db)
        .unwrap()
        .execute(
            "UPDATE mirror_threads SET codex_thread_id=?",
            params!["different"],
        )
        .unwrap();
    assert!(aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Steer)).is_err());
}

#[test]
fn unknown_dispatch_survives_error_reopen_and_never_becomes_pending() {
    let (_dir, db, id) = fixture();
    queue::complete(&db, "origin").unwrap();
    aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).unwrap();
    aq::record_error(&db, &id, "transport accepted input but response was lost").unwrap();
    assert_eq!(aq::get(&db, &id).unwrap().state, "dispatching");
    assert_eq!(
        queue::list(&db).unwrap()[0].state,
        queue::QueueJobState::Quarantined
    );
    assert!(aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).is_err());
}

#[test]
fn unfinished_original_job_prevents_completed_reply_admission() {
    let (_dir, db, id) = fixture();
    assert!(aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).is_err());
    assert_eq!(queue::list(&db).unwrap().len(), 1);
    assert_eq!(aq::get(&db, &id).unwrap().state, "open");
}

#[test]
fn unanswered_question_protects_room_and_only_verified_new_owner_or_turn_expires_it() {
    let (_dir, db, id) = fixture();
    queue::complete(&db, "origin").unwrap();
    assert_eq!(
        cdr_store::room_cleanup::pending_reason(&db, 20, Some("thread")).unwrap(),
        Some("unanswered or unconfirmed async question")
    );
    aq::retire_old_owner(&db, "resident-a", 1).unwrap();
    assert_eq!(aq::get(&db, &id).unwrap().state, "open");
    aq::supersede(&db, "resident-a", 1, "thread", "original").unwrap();
    assert_eq!(aq::get(&db, &id).unwrap().state, "open");
    aq::supersede(&db, "resident-a", 1, "thread", "newer").unwrap();
    assert_eq!(aq::get(&db, &id).unwrap().state, "expired");
    assert_eq!(
        cdr_store::room_cleanup::pending_reason(&db, 20, Some("thread")).unwrap(),
        None
    );
    aq::compact_terminal(&db, 99_999_999_999.0).unwrap();
    assert_eq!(aq::get(&db, &id).unwrap().state, "expired");
    assert!(aq::get(&db, &id).unwrap().body.options.is_empty());
}

#[test]
fn unknown_dispatch_is_not_expired_or_compacted_by_owner_change_or_ttl() {
    let (_dir, db, id) = fixture();
    aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Steer)).unwrap();
    aq::retire_old_owner(&db, "resident-b", 2).unwrap();
    aq::compact_terminal(&db, 99_999_999_999.0).unwrap();
    let q = aq::get(&db, &id).unwrap();
    assert_eq!(q.state, "dispatching");
    assert_eq!(q.body.options, ["허용", "보류"]);
}

#[test]
fn early_successor_question_waits_for_exact_reply_acceptance() {
    let (_dir, db, id) = fixture();
    queue::complete(&db, "origin").unwrap();
    aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).unwrap();
    let body = aq::get(&db, &id).unwrap().body;
    let successor = aq::observe(
        &db,
        &aq::NewQuestion {
            runtime_id: "resident-a",
            generation: 1,
            thread_id: "thread",
            turn_id: "accepted-successor",
            item_id: "early-call",
            body: &body,
            now: 3.0,
        },
    )
    .unwrap();
    assert!(aq::get(&db, &successor).is_err());
    assert!(aq::bind_receipt(&db, &successor, true).is_err());
    aq::confirm_dispatch(&db, &id, "accepted-successor").unwrap();
    assert_eq!(aq::reconcile_observations(&db, "resident-a", 1).unwrap(), 1);
    let q = aq::get(&db, &successor).unwrap();
    assert!(aq::owner_confirmed(&db, &q).unwrap());
}

#[path = "support/integrated_async_store.rs"]
mod integrated;

#[path = "support/async_text_answer.rs"]
mod text_answer;

#[test]
fn async_start_requires_baseline_and_seals_it_before_dispatch() {
    let (_dir, db, id) = fixture();
    queue::complete(&db, "origin").unwrap();
    for baseline in [
        vec![],
        vec!["other"],
        vec!["original", "original"],
        vec!["original", " "],
    ] {
        let mut c = claim(&id, aq::DispatchMode::Start);
        c.baseline_turn_ids = baseline.into_iter().map(str::to_owned).collect();
        assert!(aq::begin_dispatch(&db, &c).is_err());
        assert!(queue::list(&db).unwrap().is_empty());
        assert_eq!(aq::get(&db, &id).unwrap().state, "open");
    }
    aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).unwrap();
    aq::validate_dispatch_guards(&db, "thread").unwrap();
    open_initialized(&db)
        .unwrap()
        .execute("UPDATE codex_turn_queue SET baseline_turn_ids='[]'", [])
        .unwrap();
    assert!(aq::validate_dispatch_guards(&db, "thread").is_err());
    assert!(aq::confirm_dispatch(&db, &id, "answer-turn").is_err());
    assert_eq!(aq::get(&db, &id).unwrap().state, "dispatching");
}
