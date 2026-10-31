use crate::test_support::archive_app_fixture as app;
use crate::test_support::{approval_http, message_fixture::MessageFixture};
use std::sync::Arc;

#[tokio::test]
async fn admitted_lifecycle_commands_reject_changed_room_before_any_rpc() {
    for (command, selected) in [
        ("!archive", false),
        ("!resume", false),
        ("!archive", true),
        ("!resume", true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let http = approval_http::start().await;
        let client = Arc::new(
            twilight_http::Client::builder()
                .proxy(http.address, true)
                .ratelimiter(None)
                .build(),
        );
        let server = Arc::new(app::start(&root, &root.path().join("state.sqlite"), "normal").await);
        let fixture = MessageFixture::with_server(&root, client, server.clone());
        let db = fixture.executor.mirror_db();
        if selected {
            cdr_store::mapping::upsert_thread(db, "thread-b", "project", "b", 100, 43, 2.0)
                .unwrap();
            fixture
                .executor
                .bridge_state
                .set_selected_thread_id(Some("thread-b"))
                .unwrap();
        }
        let work = fixture.admit(command);
        let before = cdr_store::ingress::get(db, "message:801").unwrap().unwrap();
        assert_eq!(before.target_thread_id.as_deref(), Some("thread-b"));
        if selected {
            fixture
                .executor
                .bridge_state
                .set_selected_thread_id(Some("thread-a"))
                .unwrap();
        } else {
            cdr_store::mapping::upsert_thread(db, "thread-b", "project", "b", 100, 43, 2.0)
                .unwrap();
            cdr_store::mapping::upsert_thread(db, "thread-a", "project", "a", 100, 42, 2.0)
                .unwrap();
        }
        let result = crate::message_worker::process_admitted_gateway_message(
            work,
            &fixture.context(root.path()),
        )
        .await;
        server.close().await.unwrap();
        http.stop.send(()).unwrap();
        let traffic = http.task.await.unwrap();
        let after = cdr_store::ingress::get(db, "message:801").unwrap().unwrap();
        assert_eq!(before.payload, after.payload);
        assert_eq!(before.target_thread_id, after.target_thread_id);
        let calls = app::calls(&root);
        assert!(
            calls.iter().all(|v| v["method"] == "initialize"),
            "retargeted lifecycle RPC for {command}: {calls:?}"
        );
        assert!(
            result.is_err(),
            "changed target must be rejected: {command}"
        );
        assert!(
            traffic.is_empty(),
            "no success response after target change"
        );
        assert!(
            cdr_codex_state::CodexThreadStore::open(root.path().join("state.sqlite"))
                .unwrap()
                .load_thread("thread-a", false)
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn lifecycle_unchanged_and_explicit_routes_work_but_legacy_binding_is_not_invented() {
    for (command, mode) in [
        ("!archive", "same"),
        ("!resume", "same"),
        ("!archive thread-b", "explicit"),
        ("!resume thread-b", "explicit"),
        ("!archive 2", "ordinal"),
        ("!resume 2", "ordinal"),
        ("!archive", "legacy"),
        ("!resume", "legacy"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let http = approval_http::start().await;
        let client = Arc::new(
            twilight_http::Client::builder()
                .proxy(http.address, true)
                .ratelimiter(None)
                .build(),
        );
        let server = Arc::new(app::start(&root, &root.path().join("state.sqlite"), "normal").await);
        let fixture = MessageFixture::with_server(&root, client, server.clone());
        let work = fixture.admit(command);
        let db = fixture.executor.mirror_db();
        if mode == "ordinal" {
            rusqlite::Connection::open(root.path().join("state.sqlite"))
                .unwrap()
                .execute("UPDATE threads SET updated_at=30 WHERE id='thread-b'", [])
                .unwrap();
        }
        if mode == "explicit" {
            cdr_store::mapping::upsert_thread(db, "thread-b", "project", "b", 100, 43, 2.0)
                .unwrap();
            cdr_store::mapping::upsert_thread(db, "thread-a", "project", "a", 100, 42, 2.0)
                .unwrap();
        }
        if mode == "legacy" {
            rusqlite::Connection::open(db).unwrap().execute(
                "UPDATE discord_ingress_journal SET payload_json=json_remove(payload_json,'$.lifecycle_binding') WHERE ingress_id='message:801'", [],
            ).unwrap();
        }
        let before = cdr_store::ingress::get(db, "message:801").unwrap().unwrap();
        let result = crate::message_worker::process_admitted_gateway_message(
            work,
            &fixture.context(root.path()),
        )
        .await;
        server.close().await.unwrap();
        http.stop.send(()).unwrap();
        let traffic = http.task.await.unwrap();
        let after = cdr_store::ingress::get(db, "message:801").unwrap().unwrap();
        assert_eq!(before.payload, after.payload);
        assert_eq!(before.target_thread_id, after.target_thread_id);
        let calls = app::calls(&root);
        if mode == "legacy" {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("target was not frozen")
            );
            assert!(calls.iter().all(|v| v["method"] == "initialize"));
            assert!(traffic.is_empty());
        } else {
            result.unwrap();
            assert_eq!(traffic.len(), 1);
            if command.starts_with("!archive") {
                assert!(calls.iter().any(|v| v["method"] == "thread/archive"));
                assert!(!calls.iter().any(|v| matches!(
                    v["method"].as_str(),
                    Some("thread/read" | "thread/resume")
                )));
            } else {
                assert!(calls.iter().any(|v| v["method"] == "thread/read"));
            }
            for call in calls.iter().filter(|v| {
                matches!(
                    v["method"].as_str(),
                    Some("thread/read" | "thread/resume" | "thread/archive")
                )
            }) {
                assert_eq!(call["params"]["threadId"], "thread-b");
            }
        }
    }
}

#[tokio::test]
async fn archive_fenced_room_still_delivers_help_and_saved_request_inspection() {
    for command in ["!help", "!runners", "!runners message:800"] {
        let root = tempfile::tempdir().unwrap();
        let http = approval_http::start().await;
        let client = Arc::new(
            twilight_http::Client::builder()
                .proxy(http.address, true)
                .ratelimiter(None)
                .build(),
        );
        let server = Arc::new(app::start(&root, &root.path().join("state.sqlite"), "normal").await);
        let fixture = MessageFixture::with_server(&root, client, server.clone());
        let db = fixture.executor.mirror_db();
        cdr_store::archive_fence::reserve(db, &["thread-b".into()].into(), None).unwrap();
        drop(fixture.admit_id("보류 원문", 800));
        let held = cdr_store::ingress::get(db, "message:800").unwrap().unwrap();
        assert_eq!(held.state, "held");
        assert!(held.hold_reason.contains("archive scope"));
        let inspection = fixture.admit(command);
        crate::message_worker::process_admitted_gateway_message(
            inspection,
            &fixture.context(root.path()),
        )
        .await
        .unwrap();
        server.close().await.unwrap();
        http.stop.send(()).unwrap();
        let traffic = http.task.await.unwrap();
        assert!(
            !traffic.is_empty(),
            "{command} must remain usable in a held room"
        );
        assert_eq!(
            cdr_store::ingress::get(db, "message:800").unwrap().unwrap(),
            held
        );
        assert!(
            app::calls(&root)
                .iter()
                .all(|v| v["method"] == "initialize")
        );
        assert!(cdr_store::queue::list(db).unwrap().is_empty());
    }
}
