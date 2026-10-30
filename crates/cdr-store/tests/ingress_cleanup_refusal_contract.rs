use cdr_store::{
    dead_generation, delivery,
    ingress::{self, IngressKind, NewIngress},
};
use serde_json::{Value, json};

fn request() -> NewIngress {
    NewIngress {
        ingress_id: "message:700".into(),
        kind: IngressKind::Message,
        event_id: Some(700),
        application_id: Some(1),
        channel_id: 42,
        owner_user_id: 3,
        source_message_id: Some(700),
        payload: json!({"content":"!mirror sync"}),
        target_thread_id: Some("origin".into()),
        canonical_owner: None,
        now: 100.0,
    }
}

fn refusal() -> Value {
    json!({"kind":"mirror_cleanup_refused","version":1,"sync_completed":false,
        "blocked_room_id":31,"protection_reason":"ingress","delete_dispatched":false,
        "earlier_changes_possible":true})
}

#[test]
fn mc_8_known_refusal_survives_restart_without_false_completion_or_second_notice() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("mirror.sqlite");
    dead_generation::activate_runtime(&db, "old").unwrap();
    ingress::admit(&db, &request()).unwrap();
    ingress::begin_execution(&db, "message:700", "processing", None, 101.0).unwrap();
    ingress::record_result(&db, "message:700", &refusal(), 102.0).unwrap();
    dead_generation::activate_runtime(&db, "new").unwrap();
    assert_eq!(
        ingress::recover_prior_runtime(&db, "new", 103.0).unwrap(),
        1
    );
    let record = ingress::get(&db, "message:700").unwrap().unwrap();
    assert_eq!(record.outcome, Some(refusal()));
    assert_eq!(record.payload, request().payload);
    assert!(!record.confirmation_delivered);
    assert!(
        record.hold_reason.contains("room 31"),
        "{}",
        record.hold_reason
    );
    assert!(!record.hold_reason.contains("action completed"));
    assert!(
        delivery::list_pending(&db).unwrap().is_empty(),
        "known refusal must not get a second Saved POST"
    );
    ingress::hold(&db, "message:700", "late drop", false, 104.0).unwrap();
    assert_eq!(
        ingress::get(&db, "message:700").unwrap().unwrap().outcome,
        Some(refusal())
    );
    assert!(delivery::list_pending(&db).unwrap().is_empty());
    assert!(!ingress::admit(&db, &request()).unwrap().created);
    assert!(!ingress::begin_execution(&db, "message:700", "processing", None, 105.0).unwrap());
    let connection = rusqlite::Connection::open(&db).unwrap();
    assert!(
        !connection
            .query_row(
                "SELECT notice_staged FROM discord_ingress_journal WHERE ingress_id='message:700'",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
    );
}

#[test]
fn mc_8_unknown_or_malformed_outcomes_keep_the_existing_saved_notice() {
    for outcome in [
        json!({"response":"done"}),
        json!({"kind":"mirror_cleanup_refused"}),
        json!({"kind":"async_answer_not_dispatched"}),
        json!({"kind":"async_answer_not_dispatched","action_completed":false,
            "answer_dispatched":true,"thread_id":"target","question_id":"a".repeat(64),
            "reason":"not proven"}),
        {
            let mut value = refusal();
            value["delete_dispatched"] = json!(true);
            value
        },
        {
            let mut value = refusal();
            value["blocked_room_id"] = json!(0);
            value
        },
    ] {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("mirror.sqlite");
        ingress::admit(&db, &request()).unwrap();
        ingress::record_result(&db, "message:700", &outcome, 101.0).unwrap();
        ingress::hold(&db, "message:700", "unknown boundary", false, 102.0).unwrap();
        assert_eq!(delivery::list_pending(&db).unwrap().len(), 1);
    }
}
