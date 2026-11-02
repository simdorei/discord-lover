use crate::test_support::{approval_http, message_fixture::MessageFixture};
use cdr_store::{async_resolution, delivery, ingress, queue};
use std::{sync::Arc, time::Duration};

use crate::async_orphan_tests::submitted_fixture::original as async_fixture;
use crate::test_support::archive_app_fixture as app;

fn settled_question(db: &std::path::Path) {
    cdr_store::schema::open_initialized(db)
        .unwrap()
        .execute(
            "DELETE FROM mirror_threads WHERE codex_thread_id='thread-b'",
            [],
        )
        .unwrap();
    async_fixture::dispatching(db, "resident");
    async_resolution::record_terminal_notification(
        db,
        "thread-b",
        "original",
        1,
        "resident",
        r#"{"threadId":"thread-b","turn":{"id":"original","status":"completed"}}"#,
    )
    .unwrap();
    let owner = queue::list(db).unwrap().remove(0);
    delivery::stage_owned_queue_completion_with_release(
        db,
        &owner,
        "Final",
        4.0,
        Some(("resident", 1)),
    )
    .unwrap();
    delivery::complete(db, "origin").unwrap();
    cdr_store::mapping::upsert_thread(db, "thread-b", "project", "title", 100, 42, 5.0).unwrap();
    assert!(!async_resolution::admission_held(db, "thread-b").unwrap());
}

async fn check(scenario: &str, pending: bool, expected_archive_calls: usize) {
    let root = tempfile::tempdir().unwrap();
    let http = approval_http::start().await;
    let client = Arc::new(
        twilight_http::Client::builder()
            .proxy(http.address, true)
            .ratelimiter(None)
            .build(),
    );
    let server = Arc::new(app::start(&root, &root.path().join("state.sqlite"), scenario).await);
    let mut fixture = MessageFixture::with_server(&root, client, server.clone());
    fixture.executor = fixture
        .executor
        .with_app_server_resume_timeout(Duration::from_millis(500));
    let db = fixture.executor.mirror_db().to_path_buf();
    settled_question(&db);
    if pending {
        async_fixture::pending(&db, "kept-input", "thread-b", 1);
    }
    let jobs = queue::list(&db).unwrap();
    let work = fixture.admit("!archive");
    let before = ingress::get(&db, "message:801").unwrap().unwrap();
    let result = crate::message_worker::process_admitted_gateway_message(
        work,
        &fixture.context(root.path()),
    )
    .await;
    server.close().await.unwrap();
    http.stop.send(()).unwrap();
    let traffic = http.task.await.unwrap();
    assert!(
        result.is_err(),
        "a refused/unverified archive must remain an error"
    );
    assert!(
        traffic.is_empty(),
        "this path must not report archive success"
    );
    let calls = app::calls(&root);
    assert_eq!(
        calls
            .iter()
            .filter(|v| v["method"] == "thread/archive")
            .count(),
        expected_archive_calls
    );
    assert!(
        !calls
            .iter()
            .any(|v| matches!(v["method"].as_str(), Some("turn/start" | "thread/fork")))
    );
    assert_eq!(
        queue::list(&db).unwrap(),
        jobs,
        "saved input must remain unchanged"
    );
    let after = ingress::get(&db, "message:801").unwrap().unwrap();
    assert_eq!(after.payload, before.payload);
    assert!(
        !after.confirmation_delivered,
        "no Discord confirmation was delivered"
    );
    if expected_archive_calls == 0 {
        assert_eq!(
            after
                .outcome
                .as_ref()
                .and_then(|v| v.get("archive_dispatched")),
            Some(&serde_json::json!(false)),
            "known pre-dispatch failure must be durable before custody ends"
        );
        assert!(
            !async_resolution::admission_held(&db, "thread-b").unwrap(),
            "a known failed archive must not lock later questions"
        );
    } else {
        assert!(
            after.outcome.is_none(),
            "unknown archive effect cannot be relabeled as a refusal"
        );
        assert!(cdr_store::archive_fence::target_is_fenced(&db, "thread-b").unwrap());
        assert!(async_resolution::admission_held(&db, "thread-b").unwrap());
    }
    ingress::recover_prior_runtime(&db, "replacement-runtime", 1000.0).unwrap();
    assert_eq!(
        async_resolution::admission_held(&db, "thread-b").unwrap(),
        expected_archive_calls != 0
    );
    assert_eq!(queue::list(&db).unwrap(), jobs);
}

#[tokio::test]
async fn pending_work_does_not_prevent_archive_but_unknown_outcome_stays_held() {
    check("stall_archive", true, 1).await;
}

#[tokio::test]
async fn archive_refusal_deadline_before_dispatch_does_not_hold_later_input() {
    check("stall_list", false, 0).await;
}

#[tokio::test]
async fn archive_refusal_does_not_clear_uncertain_dispatched_archive() {
    check("stall_archive", false, 1).await;
}
