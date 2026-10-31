use cdr_store::{
    archive_fence,
    ingress::{self, IngressKind, NewIngress},
    schema,
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, Barrier},
};

fn request(target: Option<&str>) -> NewIngress {
    NewIngress {
        ingress_id: "message:70".into(),
        kind: IngressKind::Message,
        event_id: Some(70),
        application_id: None,
        channel_id: 99,
        owner_user_id: 20,
        source_message_id: Some(70),
        payload: json!({"version":1,"content":"  보존할 요청\n원문  ","plan":{"Execute":{"Ask":{"prompt":"saved"}}}}),
        target_thread_id: target.map(str::to_owned),
        canonical_owner: None,
        now: 3.0,
    }
}

fn scope() -> BTreeSet<String> {
    ["root".into(), "child".into()].into()
}

fn count(db: &Path) -> i64 {
    schema::open_initialized(db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM codex_archive_fences", [], |r| {
            r.get(0)
        })
        .unwrap()
}

#[test]
fn explicit_archive_wins_admission_race_without_discarding_saved_input() {
    for index in 0..20 {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("db.sqlite");
        drop(schema::open_initialized(&db).unwrap());
        let request = request(Some(if index % 2 == 0 { "root" } else { "child" }));
        let gate = Arc::new(Barrier::new(2));
        let archive = {
            let db = db.clone();
            let gate = gate.clone();
            std::thread::spawn(move || {
                gate.wait();
                archive_fence::reserve(&db, &scope(), None)
            })
        };
        gate.wait();
        let record = ingress::admit(&db, &request).unwrap().record.unwrap();
        let reservation = archive.join().unwrap();
        assert_eq!(record.payload, request.payload);
        reservation.unwrap();
        assert_eq!(count(&db), 2);
        match ingress::begin_execution(&db, "message:70", "processing", None, 4.0) {
            Ok(started) => assert!(!started),
            Err(error) => assert!(error.to_string().contains("archive fence"), "{error}"),
        }
    }
}

#[test]
fn archive_fences_survive_reopen_and_do_not_allow_direct_handoff_or_retargeting() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("db.sqlite");
    let operation = archive_fence::reserve(&db, &scope(), None).unwrap();
    let request = request(Some("child"));
    ingress::admit(&db, &request).unwrap();
    ingress::recover_prior_runtime(&db, "new-runtime", 100_000.0).unwrap();
    assert_eq!(count(&db), 2);
    let queue = cdr_store::queue::NewQueueJob {
        job_id: "q",
        target_thread_id: "child",
        channel_id: 99,
        owner_user_id: Some(20),
        discord_message_id: Some(70),
        app_server_generation: 1,
        prompt: "saved",
        queued: true,
        ack_sent: false,
        created_at: 4.0,
    };
    assert!(
        cdr_store::queue::enqueue(&db, queue)
            .unwrap_err()
            .to_string()
            .contains("archive fence")
    );
    let intake = cdr_store::prompt_intake::NewPromptIntake {
        job_id: "i",
        target_thread_id: "child",
        channel_id: 99,
        owner_user_id: Some(20),
        discord_message_id: Some(70),
        raw_prompt: "saved",
        auto_queue_when_busy: true,
        require_current_mirror: false,
        created_at: 4.0,
    };
    assert!(
        cdr_store::prompt_intake::admit_prompt_intake(&db, intake)
            .unwrap_err()
            .to_string()
            .contains("archive fence")
    );
    let connection = schema::open_initialized(&db).unwrap();
    assert!(connection.execute("UPDATE discord_ingress_journal SET state='executing' WHERE ingress_id='message:70'", []).is_err());
    assert!(
        cdr_store::queue::enqueue(
            &db,
            cdr_store::queue::NewQueueJob {
                target_thread_id: "other",
                discord_message_id: None,
                ..queue
            }
        )
        .is_ok()
    );
    assert!(
        connection
            .execute(
                "UPDATE codex_turn_queue SET target_thread_id='root' WHERE job_id='q'",
                []
            )
            .is_err()
    );
    assert!(
        cdr_store::prompt_intake::admit_prompt_intake(
            &db,
            cdr_store::prompt_intake::NewPromptIntake {
                target_thread_id: "other",
                discord_message_id: None,
                ..intake
            }
        )
        .is_ok()
    );
    assert!(
        connection
            .execute(
                "UPDATE codex_prompt_intakes SET target_thread_id='root' WHERE job_id='i'",
                []
            )
            .is_err()
    );
    archive_fence::verified(&db, &operation).unwrap();
    ingress::hold(&db, "message:70", "generic processing failure", true, 5.0).unwrap();
    let held = ingress::get(&db, "message:70").unwrap().unwrap();
    assert_eq!(held.payload, request.payload);
    assert_eq!(held.state, "held");
    assert!(held.hold_reason.contains("archive scope"));
    assert_eq!(count(&db), 2);
}

#[test]
fn archive_inspection_remains_usable_but_unknown_work_is_held_until_outcome_known() {
    for (kind, plan) in [
        (
            IngressKind::Message,
            json!({"version":1,"plan":{"Execute":"Runners"}}),
        ),
        (
            IngressKind::Message,
            json!({"version":1,"plan":{"Execute":{"SavedRequest":{"request_id":"old"}}}}),
        ),
        (
            IngressKind::Message,
            json!({"version":1,"plan":{"Execute":"Help"}}),
        ),
        (
            IngressKind::Interaction,
            json!({"version":1,"work":{"Slash":{"name":"runners"}}}),
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("db.sqlite");
        archive_fence::reserve(&db, &scope(), None).unwrap();
        let mut inspection = request(Some("root"));
        inspection.kind = kind;
        inspection.payload = plan;
        let row = ingress::admit(&db, &inspection).unwrap().record.unwrap();
        assert_eq!(row.state, "staged");
        assert!(
            ingress::begin_execution(&db, &row.ingress_id, "processing", Some("root"), 4.0)
                .unwrap()
        );
    }
    for verified in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("db.sqlite");
        let operation = archive_fence::reserve(&db, &scope(), None).unwrap();
        if verified {
            archive_fence::verified(&db, &operation).unwrap();
        }
        let row = ingress::admit(&db, &request(None)).unwrap().record.unwrap();
        assert_eq!(row.state, if verified { "staged" } else { "held" });
        if verified {
            assert!(
                ingress::begin_execution(&db, &row.ingress_id, "processing", Some("root"), 4.0)
                    .unwrap_err()
                    .to_string()
                    .contains("archive fence")
            );
        }
    }
}

#[test]
fn missing_archive_extension_is_backed_up_and_repaired_without_changing_shared_v2() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("db.sqlite");
    let mut connection = schema::open_initialized(&db).unwrap();
    connection
        .execute_batch("DROP TRIGGER cdr_archive_queue_insert_v1;")
        .unwrap();
    let backup = schema::initialize(&mut connection, &db).unwrap().unwrap();
    assert!(backup.is_file());
    assert_eq!(schema::schema_version(&connection).unwrap(), 2);
    assert!(schema::initialize(&mut connection, &db).unwrap().is_none());
    let previous = rusqlite::Connection::open(backup).unwrap();
    assert_eq!(
        previous
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE name='cdr_archive_queue_insert_v1'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn a_new_explicit_archive_is_admitted_through_an_old_archive_fence() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("db.sqlite");
    let first = archive_fence::reserve(&db, &scope(), None).unwrap();
    let mut next = request(Some("root"));
    next.payload = json!({"version":1,"author_is_bot":false,"content":"!archive",
        "plan":{"Execute":{"Archive":{"reference":null}}},
        "lifecycle_binding":{"target":"root","route":"Mapped","command":{"Archive":{"reference":null}}}});
    let admitted = ingress::admit(&db, &next).unwrap().record.unwrap();
    assert_eq!(admitted.state, "staged");
    assert!(
        ingress::begin_execution(&db, &admitted.ingress_id, "processing", Some("root"), 4.0)
            .unwrap()
    );
    let second = archive_fence::reserve(&db, &scope(), Some(&admitted.ingress_id)).unwrap();
    assert_ne!(first, second);
    assert_eq!(count(&db), 2);
}

#[test]
fn rejected_manual_retry_preserves_prior_uncertainty_and_success_can_verify_it() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("db.sqlite");
    let first = archive_fence::reserve(&db, &["root".into()].into(), None).unwrap();
    let retry = archive_fence::reserve(&db, &scope(), None).unwrap();
    archive_fence::release_rejected(&db, &retry).unwrap();
    assert_eq!(count(&db), 1);
    let connection = schema::open_initialized(&db).unwrap();
    let prior: (String, String) = connection
        .query_row(
            "SELECT operation_id,phase FROM codex_archive_fences WHERE target_thread_id='root'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(prior, (first.operation_id, "attempted".into()));
    let next = archive_fence::reserve(&db, &scope(), None).unwrap();
    archive_fence::verified(&db, &next).unwrap();
    let verified: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM codex_archive_fences WHERE phase='verified'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(verified, 2);
}

#[test]
fn previous_inspection_view_upgrades_with_backup_and_preserves_existing_fence() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("db.sqlite");
    archive_fence::reserve(&db, &scope(), None).unwrap();
    let mut connection = schema::open_initialized(&db).unwrap();
    connection
        .execute_batch("DROP VIEW cdr_archive_inspections_v1;")
        .unwrap();
    connection
        .execute_batch(include_str!(
            "../src/archive_fence/inspections_before_explicit_archive.sql"
        ))
        .unwrap();
    let backup = schema::initialize(&mut connection, &db).unwrap().unwrap();
    assert!(backup.is_file());
    let definition: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='cdr_archive_inspections_v1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(definition.contains("$.plan.Execute.Archive"));
    assert_eq!(count(&db), 2);
    assert!(schema::initialize(&mut connection, &db).unwrap().is_none());
}
