#[path = "legacy_schema.rs"]
mod legacy_schema;
use super::*;
use cdr_store::reserve_policy as reserve;

#[test]
fn old_policy_seal_is_compatible_but_identity_remains_required() {
    let (_dir, db, id) = fixture();
    queue::complete(&db, "origin").unwrap();
    aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).unwrap();
    let conn = open_initialized(&db).unwrap();
    let raw: String = conn
        .query_row(
            "SELECT preparation_json FROM cdr_async_questions WHERE id=?",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    let mut seal: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(seal.get("policy").is_none());
    seal["policy"] =
        serde_json::json!({"policy":{"mode":"auto","state":"unknown"},"failure_id":42});
    conn.execute(
        "UPDATE cdr_async_questions SET preparation_json=? WHERE id=?",
        rusqlite::params![seal.to_string(), id],
    )
    .unwrap();
    reserve::ensure(&db, "thread").unwrap();
    reserve::mark_unknown(&db, "thread", "historical unconfirmed mutation").unwrap();
    aq::validate_dispatch_guards(&db, "thread").unwrap();
    assert!(aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).is_err());
    seal.as_object_mut().unwrap().remove("identity");
    conn.execute(
        "UPDATE cdr_async_questions SET preparation_json=? WHERE id=?",
        rusqlite::params![seal.to_string(), id],
    )
    .unwrap();
    assert!(aq::validate_dispatch_guards(&db, "thread").is_err());
    assert!(aq::confirm_dispatch(&db, &id, "accepted").is_err());
}

#[test]
fn exact_job_mutation_cannot_be_confirmed_or_deleted_by_an_old_question() {
    let (_dir, db, id) = fixture();
    queue::complete(&db, "origin").unwrap();
    aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).unwrap();
    open_initialized(&db)
        .unwrap()
        .execute(
            "UPDATE codex_turn_queue SET prompt='another owner input'",
            [],
        )
        .unwrap();
    let changed = queue::list(&db).unwrap();
    assert!(aq::validate_dispatch_guards(&db, "thread").is_err());
    assert!(aq::confirm_dispatch(&db, &id, "accepted").is_err());
    assert!(aq::reject_definite(&db, &id, "late rejection").is_err());
    assert_eq!(queue::list(&db).unwrap(), changed);
    assert_eq!(aq::get(&db, &id).unwrap().state, "dispatching");
}

#[test]
fn usage_rejection_transaction_failure_preserves_unreplayable_answer_without_policy() {
    let (_dir, db, id) = fixture();
    queue::complete(&db, "origin").unwrap();
    aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).unwrap();
    let before = queue::list(&db).unwrap();
    open_initialized(&db).unwrap().execute_batch("CREATE TRIGGER fail_usage BEFORE UPDATE ON cdr_async_questions WHEN NEW.state='rejected' BEGIN SELECT RAISE(ABORT,'usage persistence failed'); END;").unwrap();
    assert!(aq::reject_usage_limit(&db, &id, "typed rejection").is_err());
    assert_eq!(aq::get(&db, &id).unwrap().state, "dispatching");
    assert_eq!(queue::list(&db).unwrap(), before);
    open_initialized(&db)
        .unwrap()
        .execute_batch("DROP TRIGGER fail_usage")
        .unwrap();
    aq::reject_usage_limit(&db, &id, "typed rejection").unwrap();
    assert!(reserve::get(&db, "thread").unwrap().is_none());
    assert_eq!(aq::get(&db, &id).unwrap().state, "rejected");
    assert!(queue::list(&db).unwrap().is_empty());
    assert!(aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).is_err());
}

#[test]
fn manual_and_off_typed_rejection_do_not_create_an_automatic_fence() {
    for mode in ["manual", "off"] {
        let (_dir, db, id) = fixture();
        queue::complete(&db, "origin").unwrap();
        reserve::set_mode(&db, "thread", mode).unwrap();
        aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).unwrap();
        aq::reject_usage_limit(&db, &id, "typed rejection").unwrap();
        assert!(!reserve::usage_failure_unresolved(&db, "thread").unwrap());
        assert_eq!(aq::get(&db, &id).unwrap().state, "rejected");
    }
}

#[test]
fn legacy_pending_dispatch_is_preserved_without_manufacturing_a_preparation() {
    let (_dir, db, id) = fixture();
    queue::complete(&db, "origin").unwrap();
    aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).unwrap();
    let legacy = open_initialized(&db).unwrap();
    legacy_schema::remove_retirement_guards(&legacy);
    legacy.execute_batch(
            // Reconstruct the actual pre-obligation schema in this temporary DB.
            // Modern dependent triggers cannot exist in a pre-seal fixture.
            "DROP TRIGGER cdr_async_obligation_question;
             DROP TRIGGER cdr_async_obligation_queue_delete;
             DROP TRIGGER cdr_async_obligation_attempt;
             DROP TRIGGER cdr_async_obligation_retention;
             DROP TRIGGER IF EXISTS cdr_async_source_no_delete; DROP TRIGGER IF EXISTS cdr_async_source_seal_immutable; DROP TRIGGER IF EXISTS cdr_async_uncopied_origin_guard; DROP TABLE IF EXISTS cdr_async_terminal_candidates; DROP VIEW cdr_async_unsettled_obligations; DROP TABLE cdr_async_terminal_settlements; DROP TABLE cdr_async_execution_handoffs; DROP TABLE cdr_async_execution_obligations;
             ALTER TABLE cdr_async_questions DROP COLUMN preparation_json;",
        )
        .unwrap();
    assert!(aq::validate_dispatch_guards(&db, "thread").is_err());
    assert_eq!(aq::get(&db, &id).unwrap().state, "dispatching");
    assert_eq!(
        queue::list(&db).unwrap()[0].state,
        queue::QueueJobState::Quarantined
    );
    assert!(aq::confirm_dispatch(&db, &id, "guessed-acceptance").is_err());
}

#[test]
fn failed_acceptance_commit_preserves_one_unreplayable_reply() {
    let (_dir, db, id) = fixture();
    queue::complete(&db, "origin").unwrap();
    aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).unwrap();
    let before = queue::list(&db).unwrap();
    open_initialized(&db).unwrap().execute_batch("CREATE TRIGGER fail_confirm BEFORE UPDATE ON cdr_async_questions WHEN NEW.state='submitted' BEGIN SELECT RAISE(ABORT,'commit failure'); END;").unwrap();
    assert!(aq::confirm_dispatch(&db, &id, "accepted").is_err());
    assert_eq!(queue::list(&db).unwrap(), before);
    assert_eq!(aq::get(&db, &id).unwrap().state, "dispatching");
    assert!(aq::begin_dispatch(&db, &claim(&id, aq::DispatchMode::Start)).is_err());
}
