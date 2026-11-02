use std::collections::BTreeMap;
use std::future::ready;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use cdr_discord::interaction_access::InteractionAccessPolicy;
use cdr_store::StoreError;
use twilight_model::channel::Message;

use super::{
    ErrorReportTarget, MessageCreateBoundaryError, dispatch_message_create,
    prepare_message_create_at,
};
use crate::config::{CliOptions, RuntimeConfig};
use crate::discord_runtime::DiscordRuntimeError;
use crate::message_worker::{MessageAdmissionError, MessageWorkerError, classify_gateway_message};

#[derive(Default)]
struct Effects {
    attachment: AtomicUsize,
    action: AtomicUsize,
    reply: AtomicUsize,
    error_report: AtomicUsize,
}

impl Effects {
    fn process(&self) -> std::future::Ready<Result<(), MessageWorkerError>> {
        self.attachment.fetch_add(1, Ordering::SeqCst);
        self.action.fetch_add(1, Ordering::SeqCst);
        self.reply.fetch_add(1, Ordering::SeqCst);
        ready(Ok(()))
    }

    fn report(&self) -> std::future::Ready<()> {
        self.error_report.fetch_add(1, Ordering::SeqCst);
        ready(())
    }

    fn assert_zero(&self) {
        assert_eq!(self.attachment.load(Ordering::SeqCst), 0);
        assert_eq!(self.action.load(Ordering::SeqCst), 0);
        assert_eq!(self.reply.load(Ordering::SeqCst), 0);
        assert_eq!(self.error_report.load(Ordering::SeqCst), 0);
    }
}

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

fn message(id: u64) -> Message {
    serde_json::from_value(serde_json::json!({
        "attachments": [{
            "content_type": "text/plain", "ephemeral": false,
            "filename": "never-downloaded.txt", "id": "1",
            "proxy_url": "http://127.0.0.1:9/never", "size": 1,
            "url": "http://127.0.0.1:9/never"
        }],
        "author": {"avatar": null, "bot": false, "discriminator": "0001",
            "id": "3", "username": "tester"},
        "channel_id": "2", "content": "hello", "edited_timestamp": null,
        "embeds": [], "id": id.to_string(), "mention_everyone": false,
        "mention_roles": [], "mentions": [], "pinned": false,
        "timestamp": "2020-02-02T02:02:02.020000+00:00", "tts": false, "type": 0
    }))
    .expect("valid gateway message fixture")
}

fn processed_table_exists(database: &std::path::Path) -> bool {
    rusqlite::Connection::open(database)
        .expect("open database")
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' \
             AND name='discord_processed_messages')",
            [],
            |row| row.get(0),
        )
        .expect("inspect schema")
}

#[tokio::test]
async fn mcb_04_policy_failure_preserves_error_and_stops_all_effects() {
    let temp = tempfile::tempdir().expect("create temporary directory");
    let database = temp.path().join("future.sqlite");
    let connection = rusqlite::Connection::open(&database).expect("create future database");
    connection
        .pragma_update(None, "user_version", 999_i64)
        .expect("set unsupported schema version");
    drop(connection);
    let effects = Effects::default();
    let input = message(305);
    let report_target = ErrorReportTarget::from_message(&input);

    let error = dispatch_message_create(
        report_target,
        || {
            prepare_message_create_at(
                input,
                None,
                &config(),
                &database,
                UNIX_EPOCH + Duration::from_secs(100),
            )
        },
        |_| effects.process(),
        |_, _| effects.report(),
    )
    .await
    .expect_err("unsupported policy schema fails");
    assert!(matches!(
        error,
        DiscordRuntimeError::Store(StoreError::UnsupportedVersion {
            found: 999,
            supported: cdr_store::schema::LATEST_STORE_SCHEMA_VERSION
        })
    ));
    effects.assert_zero();
    assert!(!processed_table_exists(&database));
}

#[tokio::test]
async fn mcb_05_real_mirror_failure_cannot_reach_any_effect() {
    let temp = tempfile::tempdir().expect("create temporary directory");
    let database = temp.path().join("future.sqlite");
    let connection = rusqlite::Connection::open(&database).expect("create future database");
    connection
        .pragma_update(None, "user_version", 999_i64)
        .expect("set unsupported schema version");
    drop(connection);
    let policy = InteractionAccessPolicy {
        allow_all_channels: true,
        ..InteractionAccessPolicy::default()
    };
    let input = message(306);
    let report_target = ErrorReportTarget::from_message(&input);
    let mirror_error = classify_gateway_message(input, &database, &config(), &policy, None)
        .err()
        .expect("real mirror lookup must preserve its store error");
    let effects = Effects::default();

    let error = dispatch_message_create(
        report_target,
        || Err(MessageCreateBoundaryError::Admission(mirror_error)),
        |_| effects.process(),
        |_, _| effects.report(),
    )
    .await
    .expect_err("mirror lookup failure propagates");
    assert!(matches!(
        error,
        DiscordRuntimeError::MessageAdmission(MessageAdmissionError::Store(
            StoreError::UnsupportedVersion {
                found: 999,
                supported: cdr_store::schema::LATEST_STORE_SCHEMA_VERSION
            }
        ))
    ));
    effects.assert_zero();
    assert!(!processed_table_exists(&database));
}

#[tokio::test]
async fn mcb_06_claim_failure_preserves_error_and_stops_all_effects() {
    let temp = tempfile::tempdir().expect("create temporary directory");
    let database = temp.path().join("mirror.sqlite");
    let connection = cdr_store::schema::open_initialized(&database).expect("initialize database");
    connection
        .execute_batch(
            "CREATE TRIGGER reject_processed_claim BEFORE INSERT ON discord_processed_messages \
             BEGIN SELECT RAISE(ABORT, 'claim blocked sentinel'); END;",
        )
        .expect("install deterministic claim failure");
    drop(connection);
    let effects = Effects::default();
    let input = message(307);
    let report_target = ErrorReportTarget::from_message(&input);

    let error = dispatch_message_create(
        report_target,
        || {
            prepare_message_create_at(
                input,
                None,
                &config(),
                &database,
                UNIX_EPOCH + Duration::from_secs(100),
            )
        },
        |_| effects.process(),
        |_, _| effects.report(),
    )
    .await
    .expect_err("claim trigger failure propagates");
    assert!(matches!(
        error,
        DiscordRuntimeError::MessageAdmission(MessageAdmissionError::Store(StoreError::Database(
            _
        )))
    ));
    assert!(error.to_string().contains("claim blocked sentinel"));
    effects.assert_zero();
    let rows: i64 = rusqlite::Connection::open(&database)
        .expect("reopen database")
        .query_row(
            "SELECT COUNT(*) FROM discord_processed_messages",
            [],
            |row| row.get(0),
        )
        .expect("count claims");
    assert_eq!(rows, 0);
}
