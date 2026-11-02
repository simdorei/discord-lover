//! Exercise the actual HTTP adapter and durable receipt together, not a mocked claim.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};

#[tokio::test]
async fn independent_confirmation_survives_lost_http_response_without_repost() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("receipt.sqlite");
    let confirm_db = db.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let count = socket.read(&mut request).await.unwrap();
        assert!(request[..count].starts_with(b"POST /api/v10/channels/42/messages "));
        // The authoritative independent acknowledgement arrived before HTTP failed.
        assert!(
            delivery_receipt::confirm(
                &confirm_db,
                r#"[42,"test/terminal","thread/turn",0]"#,
                "987"
            )
            .unwrap()
        );
        drop(socket);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    let client = twilight_http::Client::builder()
        .token("test-token".into())
        .proxy(address.to_string(), true)
        .ratelimiter(None)
        .timeout(Duration::from_secs(2))
        .build();
    let chunk = IdempotentChunk {
        domain: "test/terminal",
        logical_key: "thread/turn".into(),
        chunk_index: 0,
        content: "Final\nverified body".into(),
    };
    let result = send_chunk(&db, &client, Id::new(42), &chunk).await;
    assert!(
        result.is_ok(),
        "confirmed delivery must win over a lost HTTP response: {result:?}"
    );
    send_chunk(&db, &client, Id::new(42), &chunk).await.unwrap();
    server.await.unwrap();
    assert_eq!(delivery_receipt::unknown_count(&db).unwrap(), 0);
}

#[tokio::test]
async fn intent_storage_failure_prevents_any_http_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = twilight_http::Client::builder()
        .token("test-token".into())
        .proxy(listener.local_addr().unwrap().to_string(), true)
        .ratelimiter(None)
        .timeout(Duration::from_millis(100))
        .build();
    let temp = tempfile::tempdir().unwrap();
    let chunk = IdempotentChunk {
        domain: "test/terminal",
        logical_key: "thread/turn".into(),
        chunk_index: 0,
        content: "Final\nbody".into(),
    };
    // A directory cannot be opened as a SQLite file: fail before network admission.
    let error = send_chunk(temp.path(), &client, Id::new(42), &chunk)
        .await
        .unwrap_err();
    assert!(matches!(error, CompletionWorkerError::Store(_)));
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

async fn verify_failure(status: Option<&'static str>, body: &'static str, blocked: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let (stop, mut stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = tokio::select! {
                _ = &mut stopped => break,
                accepted = listener.accept() => accepted.unwrap(),
            };
            let mut request = Vec::new();
            loop {
                let mut buffer = [0_u8; 1024];
                let count = socket.read(&mut buffer).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() < 16_384);
                if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            assert!(request.starts_with(b"POST /api/v10/channels/42/messages "));
            observed.fetch_add(1, Ordering::SeqCst);
            if let Some(status) = status {
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        }
    });
    let client = twilight_http::Client::builder()
        .token("test-token".into())
        .proxy(address.to_string(), true)
        .ratelimiter(None)
        .timeout(Duration::from_secs(2))
        .build();
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("receipt.sqlite");
    let chunk = IdempotentChunk {
        domain: "test/terminal",
        logical_key: "thread/turn".into(),
        chunk_index: 0,
        content: "Final\nverified body".into(),
    };
    for attempt in 0..3 {
        let error = tokio::time::timeout(
            Duration::from_secs(4),
            send_chunk(&db, &client, Id::new(42), &chunk),
        )
        .await
        .unwrap()
        .unwrap_err()
        .to_string();
        if attempt > 0 {
            assert!(
                error.contains(if blocked {
                    "requires correction"
                } else {
                    "outcome unknown"
                }),
                "{error}"
            );
        }
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "later processing passes must not POST again"
    );
    assert_eq!(
        delivery_receipt::blocked_count(&db).unwrap(),
        i64::from(blocked)
    );
    assert_eq!(
        delivery_receipt::unknown_count(&db).unwrap(),
        i64::from(!blocked)
    );
    stop.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn permission_rejection_stops_future_http_attempts_and_is_not_unknown() {
    verify_failure(
        Some("403 Forbidden"),
        r#"{"code":50013,"message":"Missing Permissions"}"#,
        true,
    )
    .await;
}

#[tokio::test]
async fn lost_response_stays_unknown_and_never_reposts() {
    verify_failure(None, "", false).await;
}

#[tokio::test]
async fn accepted_but_unreadable_receipt_stays_unknown_and_never_reposts() {
    verify_failure(Some("200 OK"), "not-json", false).await;
}

#[tokio::test]
async fn server_error_is_not_misclassified_as_definite_rejection() {
    verify_failure(
        Some("500 Internal Server Error"),
        r#"{"code":0,"message":"server error"}"#,
        false,
    )
    .await;
}

#[tokio::test]
async fn preloaded_chunk_is_never_posted_after_archived_cleanup_closes_admission() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("db.sqlite");
    cdr_store::mapping::upsert_thread(&db, "old", "project", "title", 21, 31, 1.0).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = twilight_http::Client::builder()
        .token("fixture".into())
        .proxy(listener.local_addr().unwrap().to_string(), true)
        .ratelimiter(None)
        .build();
    let chunk = super::IdempotentChunk {
        domain: "final-test",
        logical_key: "old".into(),
        chunk_index: 0,
        content: "preloaded original".into(),
    };
    cdr_store::archived_cleanup::prepare(
        &db,
        &cdr_store::archived_cleanup::Prepare {
            channel: 31,
            target: "old",
            parent: 21,
            guild: 1,
            origin: &serde_json::Value::Null,
            now: 2.0,
        },
        || Ok("{}".into()),
    )
    .unwrap();
    assert!(
        super::send_chunk(&db, &http, twilight_model::id::Id::new(31), &chunk)
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn stalled_http_releases_cleanup_permit_and_preserves_unknown_receipt() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("db.sqlite");
    cdr_store::mapping::upsert_thread(&db, "old", "project", "title", 21, 31, 1.0).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = twilight_http::Client::builder()
        .token("fixture".into())
        .proxy(listener.local_addr().unwrap().to_string(), true)
        .ratelimiter(None)
        .build();
    let chunk = super::IdempotentChunk {
        domain: "test/timeout",
        logical_key: "old".into(),
        chunk_index: 0,
        content: "original".into(),
    };
    let send = super::send_chunk(&db, &http, Id::new(31), &chunk);
    let close = async {
        let (_connection, _) = listener.accept().await.unwrap();
        cdr_store::archived_cleanup::prepare(
            &db,
            &cdr_store::archived_cleanup::Prepare {
                channel: 31,
                target: "old",
                parent: 21,
                guild: 1,
                origin: &serde_json::Value::Null,
                now: 2.0,
            },
            || Ok("{}".into()),
        )
        .unwrap();
        assert_eq!(
            cdr_store::archived_cleanup::effects::in_flight(&db, 31, "old").unwrap(),
            1
        );
        tokio::time::sleep(Duration::from_secs(26)).await;
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(send, close) })
            .await
            .unwrap();
    let error = result.unwrap_err().to_string();
    assert!(error.contains("deadline expired") || error.contains("send outcome unconfirmed"), "{error}");
    assert_eq!(
        cdr_store::archived_cleanup::effects::in_flight(&db, 31, "old").unwrap(),
        0
    );
    assert_eq!(rusqlite::Connection::open(&db).unwrap().query_row("SELECT count(*) FROM codex_delivery_receipts WHERE message_id IS NULL AND retryable=0", [], |r|r.get::<_,i64>(0)).unwrap(),1);
    assert!(
        super::send_chunk(&db, &http, Id::new(31), &chunk)
            .await
            .is_err()
    );
}
