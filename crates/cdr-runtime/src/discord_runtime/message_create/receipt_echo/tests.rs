use super::*;
use cdr_store::delivery_receipt::ReceiptState;
use serde_json::json;
use twilight_model::id::Id;

fn message(content: &str) -> Message {
    serde_json::from_value(json!({
        "id":"987","channel_id":"42","author":{"id":"9","username":"bridge","discriminator":"0","avatar":null,"bot":true},
        "content":content,"timestamp":"2026-10-13T18:00:00.000000+00:00","edited_timestamp":null,
        "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
        "attachments":[],"embeds":[],"pinned":false,"type":0,"components":[]
    })).unwrap()
}

fn claim(db: &Path, domain: &str, logical: &str, index: usize, content: &str) -> (String, u64) {
    let key = serde_json::to_string(&(42, domain, logical, index)).unwrap();
    assert_eq!(
        delivery_receipt::begin(db, &key, &hex::encode(Sha256::digest(content.as_bytes())))
            .unwrap(),
        ReceiptState::New
    );
    (key, message_nonce(domain, Id::new(42), logical, index))
}

#[tokio::test]
async fn late_verified_echo_adds_immutable_evidence_after_mapping_retirement_without_resend() {
    use cdr_store::archived_cleanup as cleanup;
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("receipt.sqlite");
    cdr_store::mapping::upsert_thread(&db, "old", "p", "old", 21, 42, 1.0).unwrap();
    let (key, nonce) = claim(&db, "completion/v1", "old-output", 0, "final");
    let intent = cleanup::prepare(
        &db,
        &cleanup::Prepare {
            channel: 42,
            target: "old",
            parent: 21,
            guild: 1,
            origin: &json!({}),
            now: 2.0,
        },
        || Ok("{}".into()),
    )
    .unwrap();
    let deleted = cleanup::transition(&db, &intent, "deleted", "404", 3.0).unwrap();
    cleanup::retire_mapping(&db, &deleted, 4.0).unwrap();
    assert!(!confirm(&db, &message("final"), Some(nonce + 1), Some(9)).unwrap());
    assert!(confirm(&db, &message("final"), Some(nonce), Some(9)).unwrap());
    assert!(confirm(&db, &message("final"), Some(nonce), Some(9)).unwrap());
    let hash = hex::encode(Sha256::digest(b"final"));
    assert_eq!(
        delivery_receipt::confirmed_message(&db, &key, &hash)
            .unwrap()
            .as_deref(),
        Some("987")
    );
    assert!(!delivery_receipt::confirm(&db, &key, "988").unwrap());
    let connection = rusqlite::Connection::open(&db).unwrap();
    assert!(
        connection
            .query_row(
                "SELECT message_id FROM codex_delivery_receipts WHERE receipt_key=?",
                [&key],
                |r| r.get::<_, Option<String>>(0)
            )
            .unwrap()
            .is_none()
    );
    assert!(connection.query_row("SELECT json_extract(snapshot_json,'$.message_id') FROM cdr_archived_room_records WHERE table_name='codex_delivery_receipts' AND row_key=json_array(?)",[&key],|r|r.get::<_,Option<String>>(0)).unwrap().is_none());
    assert!(
        connection
            .execute("DELETE FROM cdr_archived_room_receipt_confirmations", [])
            .is_err()
    );
    assert!(
        connection
            .execute(
                "UPDATE cdr_archived_room_receipt_confirmations SET message_id='changed'",
                []
            )
            .is_err()
    );
    assert_eq!(
        cleanup::get(&db, 42).unwrap().unwrap().phase,
        "mapping_retired"
    );
    assert!(matches!(
        delivery_receipt::begin(&db, &key, &hash).unwrap(),
        ReceiptState::Held(_)
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = std::sync::Arc::new(
        twilight_http::Client::builder()
            .token("fixture".into())
            .proxy(listener.local_addr().unwrap().to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let sender = cdr_discord::http::DiscordHttp::new(client, Id::new(9)).with_store(&db, Some(42));
    assert!(
        sender
            .send_idempotent_message(Id::new(42), "final", &[], "completion/v1", "old-output", 0)
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[test]
fn exact_gateway_acknowledgement_releases_unknown_progress_idempotently() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("receipt.sqlite");
    let (key, nonce) = claim(
        &db,
        "completion/commentary/v1",
        "thread/turn/comment",
        0,
        "progress",
    );
    assert_eq!(delivery_receipt::unknown_count(&db).unwrap(), 1);
    assert!(confirm(&db, &message("progress"), Some(nonce), Some(9)).unwrap());
    assert!(confirm(&db, &message("progress"), Some(nonce), Some(9)).unwrap());
    assert_eq!(delivery_receipt::unknown_count(&db).unwrap(), 0);
    assert_eq!(
        delivery_receipt::confirmed_message(&db, &key, &hex::encode(Sha256::digest(b"progress")))
            .unwrap()
            .as_deref(),
        Some("987")
    );
}

#[test]
fn untrusted_or_mismatched_echo_never_releases_unknown_receipt() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("receipt.sqlite");
    let (key, nonce) = claim(
        &db,
        "completion/commentary/v1",
        "thread/turn/comment",
        0,
        "progress",
    );
    assert!(!confirm(&db, &message("progress"), None, Some(9)).unwrap());
    assert!(!confirm(&db, &message("progress"), Some(nonce + 1), Some(9)).unwrap());
    assert!(!confirm(&db, &message("progress"), Some(nonce), None).unwrap());
    for variant in 0..6 {
        let mut msg = message("progress");
        match variant {
            0 => msg.author.id = Id::new(10),
            1 => msg.author.bot = false,
            2 => msg.channel_id = Id::new(43),
            3 => msg.content = "different".into(),
            4 => msg.webhook_id = Some(Id::new(10)),
            _ => msg.edited_timestamp = Some(msg.timestamp),
        }
        assert!(!confirm(&db, &msg, Some(nonce), Some(9)).unwrap());
    }
    assert_eq!(delivery_receipt::unknown_count(&db).unwrap(), 1);
    assert!(delivery_receipt::block_rejected(&db, &key, "forbidden").unwrap());
    assert!(!confirm(&db, &message("progress"), Some(nonce), Some(9)).unwrap());
}

fn legacy_request(db: &Path, response: &str) {
    ingress::admit(
        db,
        &ingress::NewIngress {
            ingress_id: "message:100".into(),
            kind: ingress::IngressKind::Message,
            event_id: Some(100),
            application_id: Some(9),
            channel_id: 42,
            owner_user_id: 7,
            source_message_id: Some(100),
            payload: json!({}),
            target_thread_id: Some("thread".into()),
            canonical_owner: None,
            now: 1.0,
        },
    )
    .unwrap();
    cdr_store::schema::open_initialized(db).unwrap().execute(
        "UPDATE discord_ingress_journal SET state='owned',owner_kind='prompt',owner_id='job' WHERE ingress_id='message:100'", [],
    ).unwrap();
    ingress::record_result(
        db,
        "message:100",
        &json!({"response":response,"waits_for_final":true}),
        2.0,
    )
    .unwrap();
}

#[test]
fn normal_acknowledgement_opens_legacy_barrier_but_error_receipt_cannot() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("receipt.sqlite");
    legacy_request(&db, "accepted");
    let (_, error_nonce) = claim(
        &db,
        "message/error/v1",
        "inbound-message/100/error-report",
        0,
        "accepted",
    );
    assert!(confirm(&db, &message("accepted"), Some(error_nonce), Some(9)).unwrap());
    assert_eq!(
        cdr_store::first_reply::pending(&db, "job")
            .unwrap()
            .as_deref(),
        Some("message:100")
    );
    let (_, nonce) = claim(
        &db,
        "message/reply/v1",
        "inbound-message/100/action-result",
        0,
        "accepted",
    );
    assert!(confirm(&db, &message("accepted"), Some(nonce), Some(9)).unwrap());
    assert_eq!(cdr_store::first_reply::pending(&db, "job").unwrap(), None);
}

#[test]
fn legacy_barrier_requires_all_saved_response_chunks_and_exact_body() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("receipt.sqlite");
    let response = "한글 답변 ".repeat(400);
    legacy_request(&db, &response);
    let (_, wrong_nonce) = claim(
        &db,
        "message/reply/v1",
        "inbound-message/101/action-result",
        0,
        "accepted",
    );
    assert!(confirm(&db, &message("accepted"), Some(wrong_nonce), Some(9)).unwrap());
    assert!(
        cdr_store::first_reply::pending(&db, "job")
            .unwrap()
            .is_some()
    );
    let chunks = split_delivery_chunks(&response, true);
    assert!(chunks.len() > 1);
    for (index, content) in chunks.iter().enumerate() {
        let (_, nonce) = claim(
            &db,
            "message/reply/v1",
            "inbound-message/100/action-result",
            index,
            content,
        );
        let mut msg = message(content);
        msg.id = Id::new(1000 + u64::try_from(index).unwrap());
        assert!(confirm(&db, &msg, Some(nonce), Some(9)).unwrap());
        assert_eq!(
            cdr_store::first_reply::pending(&db, "job")
                .unwrap()
                .is_none(),
            index + 1 == chunks.len()
        );
    }
}
