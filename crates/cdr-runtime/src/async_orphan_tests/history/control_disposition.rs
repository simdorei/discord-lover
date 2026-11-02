use super::*;
mod incomplete;
use cdr_store::ingress::{
    self, IngressKind, NewIngress,
    stop::{StopScope, accept_unresolved},
};

fn completed_history() -> Value {
    let mut value = script("completed", Some(user_input(0)));
    value["goal_result"] = json!({"goal":null});
    value
}

fn owning_queue(f: &HistoryFixture) -> QueueCoordinator<crate::app_backend::AppServerTurnBackend> {
    QueueCoordinator::new_with_admission_gate(
        f.db.clone(),
        f.backend.clone(),
        crate::restart_readiness::drain::AdmissionGate::new(),
    )
}

fn stop(f: &HistoryFixture) {
    let binding =
        json!({"target":"thread-b","route":"Mapped","command":{"Stop":{"reference":null}}});
    let receipt = accept_unresolved(
        &f.db,
        StopScope {
            target: "thread-b",
            channel: 20,
            owner: 30,
        },
        &binding,
        None,
        || Ok(()),
    )
    .unwrap()
    .unwrap();
    assert_eq!(receipt.jobs, ["next"]);
}

fn stop_facts(f: &HistoryFixture) -> Value {
    let db = cdr_store::schema::open_initialized(&f.db).unwrap();
    let receipts = db
        .prepare(
            "SELECT operation_id,revision,scope_json FROM cdr_stop_revision_receipts
        WHERE target_thread_id='thread-b' ORDER BY revision",
        )
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let holds = db
        .prepare(
            "SELECT job_id,reason,evidence_json FROM cdr_execution_holds
        WHERE target_thread_id='thread-b' ORDER BY job_id",
        )
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    json!({"receipts":receipts,"holds":holds})
}

fn assert_no_mutation(f: &HistoryFixture, jobs: &[queue::StoredQueueJob]) {
    let methods = f
        .calls()
        .iter()
        .map(|v| v["method"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(
        methods.iter().all(|method| matches!(
            method.as_str(),
            "initialize" | "thread/read" | "thread/turns/list" | "thread/goal/get"
        )),
        "unreviewed control disposition emitted mutation RPCs: {methods:?}"
    );
    assert_eq!(queue::list(&f.db).unwrap(), jobs);
    assert_eq!(
        aq::get(&f.db, &f.id).unwrap().error,
        "original send timeout"
    );
}

#[tokio::test]
async fn accepted_stop_keeps_old_pending_held_after_terminal_and_cold_reconciliation() {
    let f = HistoryFixture::new(&completed_history()).await;
    stop(&f);
    let jobs = queue::list(&f.db).unwrap();
    let facts = stop_facts(&f);
    for _ in 0..3 {
        let report = owning_queue(&f).recover_target("thread-b").await.unwrap();
        assert_eq!(report.started, 0);
        assert_eq!(f.obligation().1, "terminal");
        assert_eq!(f.obligation().2, "settled");
        assert_eq!(stop_facts(&f), facts);
        assert_no_mutation(&f, &jobs);
    }
    f.server.close().await.unwrap();
}

#[tokio::test]
async fn stop_during_original_terminal_read_is_accepted_without_waiting_for_target_lock() {
    let mut value = completed_history();
    value["terminal_gate"] = json!(true);
    let f = HistoryFixture::new(&value).await;
    let coordinator = owning_queue(&f);
    let task = tokio::spawn(async move { coordinator.recover_target("thread-b").await });
    wait_file(&f.temp.path().join("rpc.jsonl.terminal-entered")).await;
    let before = std::time::Instant::now();
    stop(&f);
    assert!(
        before.elapsed() < Duration::from_secs(3),
        "durable Stop acceptance waited for a native read"
    );
    let jobs = queue::list(&f.db).unwrap();
    let facts = stop_facts(&f);
    std::fs::write(
        f.temp.path().join("rpc.jsonl.terminal-release"),
        b"release after Stop",
    )
    .unwrap();
    assert_eq!(task.await.unwrap().unwrap().started, 0);
    assert_eq!(f.obligation().1, "terminal");
    assert_eq!(stop_facts(&f), facts);
    assert_no_mutation(&f, &jobs);
    f.server.close().await.unwrap();
}

#[tokio::test]
async fn attempted_and_verified_archive_fences_survive_read_only_terminal_settlement() {
    for phase in ["attempted", "verified"] {
        let f = HistoryFixture::new(&completed_history()).await;
        queue::complete(&f.db, "next").unwrap();
        let operation = cdr_store::archive_fence::reserve(
            &f.db,
            &std::collections::BTreeSet::from(["thread-b".into()]),
            None,
        )
        .unwrap();
        if phase == "verified" {
            cdr_store::archive_fence::verified(&f.db, &operation).unwrap();
        }
        for _ in 0..2 {
            assert_eq!(
                owning_queue(&f)
                    .recover_target("thread-b")
                    .await
                    .unwrap()
                    .started,
                0
            );
            assert_eq!(f.obligation().1, "terminal");
            let actual:(String,String)=cdr_store::schema::open_initialized(&f.db).unwrap()
                .query_row("SELECT operation_id,phase FROM codex_archive_fences WHERE target_thread_id='thread-b'",
                    [],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert_eq!(actual, (operation.operation_id.clone(), phase.to_owned()));
            assert_no_mutation(&f, &[]);
        }
        assert!(
            queue::enqueue(
                &f.db,
                queue::NewQueueJob {
                    job_id: "later",
                    target_thread_id: "thread-b",
                    channel_id: 20,
                    owner_user_id: Some(30),
                    discord_message_id: None,
                    app_server_generation: 1,
                    prompt: "never run this",
                    queued: true,
                    ack_sent: true,
                    created_at: 20.0,
                }
            )
            .is_err(),
            "an archive fence allowed a new queue handoff"
        );
        assert_no_mutation(&f, &[]);
        f.server.close().await.unwrap();
    }
}

fn unresolved_archive(f: &HistoryFixture) -> ingress::StoredIngress {
    let command = json!({"Archive":{"reference":null}});
    let request = NewIngress {
        ingress_id: "message:901".into(),
        kind: IngressKind::Message,
        event_id: Some(901),
        application_id: None,
        channel_id: 20,
        owner_user_id: 30,
        source_message_id: Some(901),
        payload: json!({"version":1,"plan":{"Execute":command.clone()},
            "lifecycle_binding":{"target":"thread-b","route":"Mapped","command":command}}),
        target_thread_id: Some("thread-b".into()),
        canonical_owner: None,
        now: 10.0,
    };
    assert!(ingress::admit(&f.db, &request).unwrap().created);
    ingress::hold(
        &f.db,
        &request.ingress_id,
        "original archive request requires disposition",
        true,
        11.0,
    )
    .unwrap();
    ingress::get(&f.db, &request.ingress_id).unwrap().unwrap()
}

#[tokio::test]
async fn unresolved_later_archive_intent_precedes_old_pending_after_terminal_recovery() {
    let f = HistoryFixture::new(&completed_history()).await;
    let intent = unresolved_archive(&f);
    let jobs = queue::list(&f.db).unwrap();
    let report = owning_queue(&f).recover_target("thread-b").await.unwrap();
    assert_eq!(report.started, 0);
    assert_eq!(f.obligation().1, "terminal");
    assert_eq!(
        ingress::get(&f.db, &intent.ingress_id).unwrap(),
        Some(intent)
    );
    assert_no_mutation(&f, &jobs);
    f.server.close().await.unwrap();
}

#[tokio::test]
async fn unresolved_archive_blocks_atomic_start_without_borrowing_control_or_other_target_authority()
 {
    let f = HistoryFixture::new(&completed_history()).await;
    queue::complete(&f.db, "next").unwrap();
    owning_queue(&f).recover_target("thread-b").await.unwrap();
    assert_eq!(f.obligation().1, "terminal");
    fixture::pending(&f.db, "next", "thread-b", 1);
    let intent = unresolved_archive(&f);
    let before = queue::list(&f.db).unwrap();
    assert!(cdr_store::async_resolution::admission_held(&f.db, "thread-b").unwrap());
    // Admission hold must not become a new generic mutation/control permission.
    // The unchanged mutation guard remains a separate, necessary check.
    cdr_store::async_resolution::guard_mutation(&f.db, "thread-b").unwrap();
    assert!(queue::try_begin_attempt(&f.db, "next", &[], 1).is_err());
    assert_eq!(queue::list(&f.db).unwrap(), before);
    assert_eq!(
        ingress::get(&f.db, &intent.ingress_id).unwrap(),
        Some(intent)
    );
    cdr_store::schema::open_initialized(&f.db)
        .unwrap()
        .execute(
            "INSERT INTO mirror_threads VALUES('other','project','other',10,21,0)",
            [],
        )
        .unwrap();
    queue::enqueue(
        &f.db,
        queue::NewQueueJob {
            job_id: "other-job",
            target_thread_id: "other",
            channel_id: 21,
            owner_user_id: Some(30),
            discord_message_id: None,
            app_server_generation: 1,
            prompt: "independent fixture request",
            queued: true,
            ack_sent: true,
            created_at: 20.0,
        },
    )
    .unwrap();
    let claimed = queue::try_begin_attempt(&f.db, "other-job", &[], 1)
        .unwrap()
        .unwrap();
    assert_eq!(claimed.state, queue::QueueJobState::Starting);
    assert_eq!(claimed.attempt_count, 1);
    assert_no_mutation(&f, &queue::list(&f.db).unwrap());
    f.server.close().await.unwrap();
}

fn admit_control(f: &HistoryFixture, target: &str, command: Value, event: i64) -> String {
    let key = format!("message:{event}");
    let mut payload = json!({"version":1,"plan":{"Execute":command},
        "lifecycle_binding":{"target":target,"route":"Mapped"}});
    payload["lifecycle_binding"]["command"] = command;
    ingress::admit(
        &f.db,
        &NewIngress {
            ingress_id: key.clone(),
            kind: IngressKind::Message,
            event_id: Some(event),
            application_id: None,
            channel_id: 20,
            owner_user_id: 30,
            source_message_id: Some(event),
            payload,
            target_thread_id: Some(target.into()),
            canonical_owner: None,
            now: 10.0,
        },
    )
    .unwrap();
    key
}

#[tokio::test]
async fn terminal_recovery_control_disposition_is_targeted_bounded_and_not_a_display_label_grant() {
    for mode in [
        "result_recorded",
        "stop_accepted",
        "foreign",
        "ordinary_request",
        "malformed",
        "oversized",
        "page_limit",
        "broken_schema",
    ] {
        let f = HistoryFixture::new(&completed_history()).await;
        queue::complete(&f.db, "next").unwrap();
        owning_queue(&f).recover_target("thread-b").await.unwrap();
        assert_eq!(f.obligation().1, "terminal");
        stage_control_disposition(&f, mode);
        let held = cdr_store::async_resolution::admission_held(&f.db, "thread-b");
        if mode == "broken_schema" {
            assert!(held.is_err());
        } else {
            assert_eq!(
                held.unwrap(),
                matches!(mode, "malformed" | "oversized" | "page_limit"),
                "{mode}"
            );
            if mode == "stop_accepted" {
                assert!(
                    queue::try_begin_attempt(&f.db, "next", &[], 1)
                        .unwrap()
                        .is_none(),
                    "accepted Stop lost the independently held original job"
                );
            }
        }
        assert!(f.calls().iter().all(|v| matches!(
            v["method"].as_str(),
            Some("initialize" | "thread/read" | "thread/turns/list" | "thread/goal/get")
        )));
        let closed = f.server.close().await;
        if mode == "broken_schema" {
            let error = closed.unwrap_err().to_string();
            assert!(
                error.contains("archive inspection definition is unknown"),
                "{error}"
            );
            assert!(f.calls().iter().all(|v| matches!(
                v["method"].as_str(),
                Some("initialize" | "thread/read" | "thread/turns/list" | "thread/goal/get")
            )));
        } else {
            closed.unwrap();
        }
    }
}

fn stage_control_disposition(f: &HistoryFixture, mode: &str) {
    if mode == "page_limit" {
        for event in 901..1031 {
            admit_control(f, "thread-b", json!({"Usage":{"days":7}}), event);
        }
    } else {
        let command = match mode {
            "stop_accepted" => json!({"Stop":{"reference":null}}),
            "ordinary_request" => json!({"Ask":{"prompt":"ordinary unowned request"}}),
            _ => json!({"Archive":{"reference":null}}),
        };
        let target = if mode == "foreign" {
            "other"
        } else {
            "thread-b"
        };
        let key = admit_control(f, target, command, 901);
        match mode {
            "result_recorded" => {
                assert!(
                    ingress::begin_execution(&f.db, &key, "processing", Some("thread-b"), 10.5)
                        .unwrap()
                );
                ingress::record_result(
                    &f.db,
                    &key,
                    &json!({"kind":"error","content":"known pre-effect refusal"}),
                    11.0,
                )
                .unwrap();
                ingress::hold(&f.db, &key, "confirmation was lost", false, 12.0).unwrap();
            }
            "stop_accepted" => {
                fixture::pending(&f.db, "next", "thread-b", 1);
                assert!(
                    ingress::begin_execution(&f.db, &key, "processing", Some("thread-b"), 10.5)
                        .unwrap()
                );
                let record = ingress::get(&f.db, &key).unwrap().unwrap();
                let binding = record.payload["lifecycle_binding"].clone();
                accept_unresolved(
                    &f.db,
                    StopScope {
                        target: "thread-b",
                        channel: 20,
                        owner: 30,
                    },
                    &binding,
                    Some(&record),
                    || Ok(()),
                )
                .unwrap()
                .unwrap();
                assert!(
                    cdr_store::execution_hold::reason(&f.db, "next")
                        .unwrap()
                        .is_some()
                );
            }
            "malformed" | "oversized" => {
                ingress::hold(&f.db, &key, "unresolved control", true, 11.0).unwrap();
                let payload = if mode == "oversized" {
                    "x".repeat(131_073)
                } else {
                    "not-json".into()
                };
                cdr_store::schema::open_initialized(&f.db)
                    .unwrap()
                    .execute(
                        "UPDATE discord_ingress_journal SET payload_json=? WHERE ingress_id=?",
                        rusqlite::params![payload, key],
                    )
                    .unwrap();
            }
            "broken_schema" => {
                cdr_store::schema::open_initialized(&f.db).unwrap().execute_batch(
                    "ALTER TABLE discord_ingress_journal RENAME COLUMN payload_json TO unavailable_payload",
                ).unwrap();
            }
            _ => {}
        }
    }
}
