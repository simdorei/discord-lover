use std::collections::BTreeMap;
use std::time::{Duration, UNIX_EPOCH};

use cdr_discord::interaction_access::InteractionAccessPolicy;
use cdr_store::StoreError;
use cdr_store::processed::is_processed;
use twilight_model::channel::Message;

use super::{MessageAdmissionError, admit_message_candidate_at};
use crate::config::{CliOptions, RuntimeConfig};
use crate::message_worker::classification::{MessageClassification, classify_gateway_message};

fn config() -> RuntimeConfig {
    RuntimeConfig::from_map(
        &BTreeMap::from([
            ("DISCORD_BOT_TOKEN".into(), "test-token".into()),
            ("DISCORD_ALLOW_ALL_CHANNELS".into(), "1".into()),
        ]),
        CliOptions::default(),
    )
    .expect("valid test config")
}

fn message(id: u64, content: &str) -> Message {
    serde_json::from_value(serde_json::json!({
        "attachments": [],
        "author": {
            "avatar": null,
            "bot": false,
            "discriminator": "0001",
            "id": "3",
            "username": "tester"
        },
        "channel_id": "2",
        "content": content,
        "edited_timestamp": null,
        "embeds": [],
        "id": id.to_string(),
        "mention_everyone": false,
        "mention_roles": [],
        "mentions": [],
        "pinned": false,
        "timestamp": "2020-02-02T02:02:02.020000+00:00",
        "tts": false,
        "type": 0
    }))
    .expect("valid gateway message fixture")
}

fn candidate(message: Message, database: &std::path::Path) -> super::MessageCandidate {
    let policy = InteractionAccessPolicy {
        allow_all_channels: true,
        ..InteractionAccessPolicy::default()
    };
    match classify_gateway_message(message, database, &config(), &policy, None)
        .expect("classification succeeds")
    {
        MessageClassification::Candidate(candidate) => candidate,
        MessageClassification::Ignore(_) => panic!("test input must be a candidate"),
    }
}

#[test]
fn mpb_01_out_of_range_id_fails_before_database_access() {
    let temp = tempfile::tempdir().expect("create temporary directory");
    let database = temp.path().join("missing-parent").join("mirror.sqlite");
    let policy = InteractionAccessPolicy {
        allow_all_channels: true,
        ..InteractionAccessPolicy::default()
    };
    let error = classify_gateway_message(
        message(u64::MAX, "ping"),
        &database,
        &config(),
        &policy,
        None,
    )
    .err()
    .expect("out-of-range ID must fail");
    assert!(matches!(error, MessageAdmissionError::IntegerRange));
    assert!(!database.exists());
}

#[test]
fn mpb_02_pre_unix_clock_fails_without_a_processed_row() {
    let temp = tempfile::tempdir().expect("create temporary directory");
    let database = temp.path().join("mirror.sqlite");
    let before_unix = UNIX_EPOCH
        .checked_sub(Duration::from_secs(1))
        .expect("platform supports pre-Unix system time");

    let error = admit_message_candidate_at(candidate(message(41, "ping"), &database), before_unix)
        .err()
        .expect("pre-Unix time must remain an admission error");
    assert!(matches!(
        error,
        MessageAdmissionError::Store(StoreError::SystemTime(_))
    ));
    assert!(!is_processed(&database, 41).expect("read replay state"));
}

#[test]
fn mpb_03_claim_failure_preserves_the_store_error_without_admission() {
    let temp = tempfile::tempdir().expect("create temporary directory");
    let database = temp.path().join("mirror.sqlite");
    let candidate = candidate(message(42, "ping"), &database);
    rusqlite::Connection::open(&database)
        .expect("open test database")
        .pragma_update(None, "user_version", 999_i64)
        .expect("install unsupported schema version");

    let error = admit_message_candidate_at(candidate, UNIX_EPOCH + Duration::from_secs(100))
        .err()
        .expect("unsupported schema must fail admission");
    assert!(matches!(
        error,
        MessageAdmissionError::Store(StoreError::UnsupportedVersion {
            found: 999,
            supported: cdr_store::schema::LATEST_STORE_SCHEMA_VERSION
        })
    ));
}

#[test]
fn mpb_04_first_candidate_gets_a_database_bound_token_and_duplicate_gets_none() {
    let temp = tempfile::tempdir().expect("create temporary directory");
    let database = temp.path().join("mirror.sqlite");
    let observed_at = UNIX_EPOCH + Duration::from_secs(100);

    let admitted =
        admit_message_candidate_at(candidate(message(43, "ping"), &database), observed_at)
            .expect("first claim succeeds")
            .expect("first copy receives a token");
    let parts = admitted
        .into_processing_parts(&database)
        .expect("matching database accepts token");
    let owned_message: Box<Message> = parts.message;
    assert_eq!(owned_message.id.get(), 43);
    assert_eq!(parts.channel_id, 2);
    assert_eq!(parts.user_id, 3);

    let duplicate = admit_message_candidate_at(
        candidate(message(43, "ping"), &database),
        observed_at + Duration::from_secs(1),
    )
    .expect("duplicate lookup succeeds");
    assert!(duplicate.is_none());
}
