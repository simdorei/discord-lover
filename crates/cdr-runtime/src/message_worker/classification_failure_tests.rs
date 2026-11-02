use std::collections::BTreeMap;

use cdr_discord::interaction_access::InteractionAccessPolicy;
use cdr_store::StoreError;
use twilight_model::channel::Message;

use super::classify_gateway_message;
use crate::config::{CliOptions, RuntimeConfig};
use crate::message_worker::MessageAdmissionError;

fn message() -> Message {
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
        "content": "hello",
        "edited_timestamp": null,
        "embeds": [],
        "id": "110",
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

#[test]
fn mcg_04_mirror_lookup_failure_preserves_error_without_a_claim() {
    let temp = tempfile::tempdir().expect("create temporary directory");
    let database = temp.path().join("future.sqlite");
    let connection = rusqlite::Connection::open(&database).expect("create future database");
    connection
        .pragma_update(None, "user_version", 999_i64)
        .expect("set unsupported schema version");
    drop(connection);
    let config = RuntimeConfig::from_map(
        &BTreeMap::from([
            ("DISCORD_BOT_TOKEN".into(), "test-token".into()),
            ("DISCORD_ALLOW_ALL_CHANNELS".into(), "1".into()),
        ]),
        CliOptions::default(),
    )
    .expect("valid test config");
    let policy = InteractionAccessPolicy {
        allow_all_channels: true,
        ..InteractionAccessPolicy::default()
    };

    let error = classify_gateway_message(message(), &database, &config, &policy, None)
        .err()
        .expect("future schema must fail mirror lookup");
    assert!(matches!(
        error,
        MessageAdmissionError::Store(StoreError::UnsupportedVersion {
            found: 999,
            supported: cdr_store::schema::LATEST_STORE_SCHEMA_VERSION
        })
    ));
    let connection = rusqlite::Connection::open(&database).expect("reopen future database");
    let processed_tables: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name='discord_processed_messages'",
            [],
            |row| row.get(0),
        )
        .expect("inspect future schema");
    assert_eq!(processed_tables, 0);
}
