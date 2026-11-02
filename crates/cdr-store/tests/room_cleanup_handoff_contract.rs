use cdr_store::{ingress, queue, room_cleanup};
use rusqlite::{Connection, params};

#[path = "support/owned_prompt.rs"]
#[allow(dead_code)]
mod owned_prompt;

#[test]
fn mc_1_settled_owned_history_allows_cleanup_without_rewriting_journal() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("mirror.sqlite");
    owned_prompt::settled(&db);
    let before = ingress::get(&db, "message:501").unwrap().unwrap();
    assert_eq!(before.state, "owned");
    assert!(before.confirmation_delivered);
    assert!(queue::list(&db).unwrap().is_empty());
    assert_eq!(
        room_cleanup::pending_reason(&db, 31, Some("target")).unwrap(),
        None,
        "MC-1: permanent ownership history is not an unfinished request"
    );
    room_cleanup::begin(&db, 31, Some("target"), 20.0).unwrap();
    let after = ingress::get(&db, "message:501").unwrap().unwrap();
    assert_eq!(after.state, before.state);
    assert_eq!(after.owner_id, before.owner_id);
    assert_eq!(after.payload, before.payload);
    assert_eq!(after.outcome, before.outcome);
    assert_eq!(after.updated_at.to_bits(), before.updated_at.to_bits());
}

#[test]
fn mc_2_handed_off_history_does_not_bypass_queue_or_final_outbox() {
    for final_pending in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("mirror.sqlite");
        if final_pending {
            owned_prompt::final_pending(&db);
        } else {
            owned_prompt::queued(&db);
        }
        assert!(
            room_cleanup::pending_reason(&db, 31, Some("target"))
                .unwrap()
                .is_some()
        );
        assert!(room_cleanup::begin(&db, 31, Some("target"), 20.0).is_err());
        assert_eq!(room_cleanup::phase(&db, 31).unwrap(), None);
    }
}

#[test]
fn mc_2_malformed_or_unconfirmed_owned_history_stays_protected() {
    for (state, kind, owner, confirmed) in [
        ("owned", Some("prompt"), Some("owned-job"), 0),
        ("owned", None, Some("owned-job"), 1),
        ("owned", Some("other"), Some("owned-job"), 1),
        ("owned", Some("prompt"), None, 1),
        ("owned", Some("prompt"), Some(""), 1),
        ("owned", Some("prompt"), Some("   "), 1),
        ("held", Some("prompt"), Some("owned-job"), 1),
        ("executing", Some("prompt"), Some("owned-job"), 1),
        ("future-state", Some("prompt"), Some("owned-job"), 1),
        ("completed", None, None, 0),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("mirror.sqlite");
        owned_prompt::settled(&db);
        // Malformed records have no public constructor: inject only at this boundary.
        let connection = Connection::open(&db).unwrap();
        connection
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        connection.execute(
            "UPDATE discord_ingress_journal SET state=?,owner_kind=?,owner_id=?,confirmation_delivered=?",
            params![state, kind, owner, confirmed],
        ).unwrap();
        assert_eq!(
            room_cleanup::pending_reason(&db, 31, Some("target")).unwrap(),
            Some("ingress"),
            "state={state}, kind={kind:?}, owner={owner:?}, confirmed={confirmed}"
        );
        assert!(room_cleanup::begin(&db, 31, Some("target"), 20.0).is_err());
    }
}

#[test]
fn mc_4_settled_history_does_not_hide_a_second_request_on_another_channel() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("mirror.sqlite");
    owned_prompt::settled(&db);
    let mut other = owned_prompt::request();
    other.ingress_id = "message:502".into();
    other.event_id = Some(502);
    other.source_message_id = Some(502);
    other.channel_id = 99;
    ingress::admit(&db, &other).unwrap();
    assert_eq!(
        room_cleanup::pending_reason(&db, 31, Some("target")).unwrap(),
        Some("ingress")
    );
    assert!(room_cleanup::begin(&db, 31, Some("target"), 20.0).is_err());
    assert_eq!(
        ingress::get(&db, "message:502").unwrap().unwrap().state,
        "staged"
    );
}

#[test]
fn mc_1_explicitly_retracted_pending_prompt_does_not_need_a_fake_final_receipt() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("mirror.sqlite");
    owned_prompt::queued(&db);
    assert!(
        queue::retract(&db, "target", Some(31), Some(42))
            .unwrap()
            .is_some()
    );
    assert_eq!(
        room_cleanup::pending_reason(&db, 31, Some("target")).unwrap(),
        None
    );
    room_cleanup::begin(&db, 31, Some("target"), 20.0).unwrap();
}

#[test]
fn mc_5_transactional_pending_refusal_is_typed_not_an_integrity_failure() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("mirror.sqlite");
    owned_prompt::queued(&db);
    let error = room_cleanup::begin(&db, 31, Some("target"), 20.0).unwrap_err();
    assert!(
        format!("{error:?}").starts_with("CleanupProtected"),
        "MC-5: only a definite pre-delete refusal has this type: {error:?}"
    );
    assert_eq!(room_cleanup::phase(&db, 31).unwrap(), None);
}

#[test]
fn mc_2_every_independent_guard_still_protects_settled_owned_history() {
    for (expected, sql) in [
        (
            "prompt intake",
            "INSERT INTO codex_prompt_intakes (job_id,target_thread_id,channel_id,raw_prompt,auto_queue_when_busy,require_current_mirror,created_at,updated_at) VALUES ('next','target',99,'pending',1,1,1,1)",
        ),
        (
            "undelivered progress",
            "INSERT INTO codex_commentary_outbox (delivery_key,job_id,target_thread_id,turn_id,channel_id,text) VALUES ('progress','owned-job','target','turn',31,'pending')",
        ),
        (
            "busy choice",
            "INSERT INTO busy_choices (choice_id,owner_user_id,channel_id,target_thread_id,prompt,allow_steer,created_at,expires_at) VALUES ('choice',42,31,'target','pending',0,1,unixepoch()+600)",
        ),
        (
            "undelivered goal progress",
            "INSERT INTO codex_goal_progress (thread,turn,channel,content) VALUES ('target','turn',31,'pending')",
        ),
        (
            "unsettled delivery receipt",
            "INSERT INTO codex_delivery_receipts (receipt_key,content_hash) VALUES ('[31,\"later\",\"job\",0]','hash')",
        ),
        (
            "unattributable delivery receipt",
            "INSERT INTO codex_delivery_receipts (receipt_key,content_hash) VALUES ('broken','hash')",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("mirror.sqlite");
        owned_prompt::settled(&db);
        Connection::open(&db).unwrap().execute_batch(sql).unwrap();
        let reason = room_cleanup::pending_reason(&db, 31, Some("target"))
            .unwrap()
            .unwrap();
        assert!(reason.starts_with(expected), "{expected}: {reason}");
        assert!(matches!(
            room_cleanup::begin(&db, 31, Some("target"), 20.0),
            Err(cdr_store::StoreError::CleanupProtected { channel: 31, .. })
        ));
        assert_eq!(room_cleanup::phase(&db, 31).unwrap(), None);
    }
}

#[test]
fn mc_10_database_and_existing_fence_failures_are_not_known_protection_refusals() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("mirror.sqlite");
    owned_prompt::settled(&db);
    Connection::open(&db).unwrap().execute_batch("ALTER TABLE codex_goal_progress RENAME COLUMN content TO changed_content; ALTER TABLE codex_goal_progress RENAME COLUMN channel TO changed_channel;").unwrap();
    assert!(matches!(
        room_cleanup::pending_reason(&db, 31, Some("target")),
        Err(cdr_store::StoreError::Database(_) | cdr_store::StoreError::Integrity(_))
    ));
    assert!(matches!(
        room_cleanup::begin(&db, 31, Some("target"), 20.0),
        Err(cdr_store::StoreError::Database(_) | cdr_store::StoreError::Integrity(_))
    ));
    assert_eq!(
        Connection::open(&db)
            .unwrap()
            .query_row("SELECT count(*) FROM cdr_cleanup_fences", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("mirror.sqlite");
    owned_prompt::settled(&db);
    room_cleanup::begin(&db, 31, Some("target"), 20.0).unwrap();
    assert!(matches!(
        room_cleanup::begin(&db, 31, Some("target"), 21.0),
        Err(cdr_store::StoreError::Integrity(_))
    ));
}
