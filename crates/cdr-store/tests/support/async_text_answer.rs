use super::{aq, claim, fixture, open_initialized};

#[test]
fn free_text_is_exact_one_shot_and_sealed_with_rollback_capability() {
    let (_dir, db, id) = fixture();
    let text = "선택지에는 없어요.\nA=B | C=보류 🙂";
    let mut c = claim(&id, aq::DispatchMode::Steer);
    c.answer = aq::Answer::Text(text.into());
    let q = aq::begin_dispatch(&db, &c).unwrap();
    assert_eq!(q.chosen, None);
    assert_eq!(q.chosen_text.as_deref(), Some(text));
    assert_eq!(aq::chosen_answer(&q).unwrap(), c.answer);
    let prompt = aq::answer_prompt_with(&q, &c.answer).unwrap();
    let data: serde_json::Value = serde_json::from_str(prompt.split_once('\n').unwrap().1).unwrap();
    assert_eq!(data["answer_text"], text);
    assert_eq!(data["question_index"], 1);
    assert_eq!(data["question_item_id"], "call-id");
    assert!(data.get("selected_option").is_none());
    assert!(aq::begin_dispatch(&db, &c).is_err());
    assert!(aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Steer)).is_err());
    let conn = open_initialized(&db).unwrap();
    let version: i64 = conn
        .query_row(
            "SELECT format_version FROM cdr_runtime_capability_requirements WHERE component=?",
            [aq::FREE_TEXT_COMPONENT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, aq::FREE_TEXT_FORMAT_VERSION);
    aq::validate_dispatch_guards(&db, "thread").unwrap();
    let history = cdr_store::async_resolution::capture_history_snapshot(&db, "thread")
        .unwrap()
        .unwrap();
    assert_eq!(history.turn_ids(), ["original"]);
    assert_eq!(history.obligation_count(), 1);
    conn.execute(
        "UPDATE cdr_async_questions SET answer_text='changed' WHERE id=?",
        [&id],
    )
    .unwrap();
    assert!(aq::validate_dispatch_guards(&db, "thread").is_err());
    assert!(aq::confirm_dispatch(&db, &id, "original").is_err());
}

#[test]
fn migration_preserves_an_existing_choice_seal_without_text_capability() {
    let (_dir, db, id) = fixture();
    aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Steer)).unwrap();
    let conn = open_initialized(&db).unwrap();
    let before: String = conn
        .query_row(
            "SELECT preparation_json FROM cdr_async_questions WHERE id=?",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute_batch("DROP TRIGGER cdr_async_obligation_question; DROP TRIGGER cdr_async_obligation_queue_delete; ALTER TABLE cdr_async_questions DROP COLUMN answer_text;").unwrap();
    drop(conn);
    let conn = open_initialized(&db).unwrap();
    let after: String = conn
        .query_row(
            "SELECT preparation_json FROM cdr_async_questions WHERE id=?",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        aq::chosen_answer(&aq::get(&db, &id).unwrap()).unwrap(),
        aq::Answer::Choice(1)
    );
    aq::validate_dispatch_guards(&db, "thread").unwrap();
    assert!(
        cdr_store::async_resolution::capture_history_snapshot(&db, "thread")
            .unwrap()
            .is_some()
    );
    aq::confirm_dispatch(&db, &id, "original").unwrap();
}

#[test]
fn invalid_text_or_wrong_question_binding_cannot_consume_question() {
    let (_dir, db, id) = fixture();
    for text in [String::new(), " \n\t".into(), "가".repeat(4001)] {
        let mut c = claim(&id, aq::DispatchMode::Steer);
        c.answer = aq::Answer::Text(text);
        assert!(aq::begin_dispatch(&db, &c).is_err());
    }
    for field in 0..4 {
        let mut c = claim(&id, aq::DispatchMode::Steer);
        c.answer = aq::Answer::Text("다른 답변".into());
        match field {
            0 => c.actor += 1,
            1 => c.channel += 1,
            2 => c.message = "9999",
            _ => c.generation += 1,
        }
        assert!(aq::begin_dispatch(&db, &c).is_err());
    }
    let q = aq::get(&db, &id).unwrap();
    assert_eq!(q.state, "open");
    assert!(q.chosen_text.is_none());
    let conn = open_initialized(&db).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM cdr_runtime_capability_requirements WHERE component=?",
            [aq::FREE_TEXT_COMPONENT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}
