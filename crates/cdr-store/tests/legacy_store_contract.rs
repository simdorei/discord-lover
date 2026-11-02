//! Legacy database compatibility, without retaining an executable Python rollback.
use cdr_store::processed::{claim, mark};
use cdr_store::prompt_intake::{NewPromptIntake, admit_prompt_intake, get_prompt_intake};
use cdr_store::schema::open_initialized;
use rusqlite::Connection;
use std::path::Path;

const FAR_FUTURE: f64 = 10_000_000_000.0;
const LEGACY_SCHEMA: &str = include_str!("../../../fixtures/parity/discord_mirror_schema_v2.sql");

fn legacy_store(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(LEGACY_SCHEMA).unwrap();
    conn
}

#[test]
fn legacy_v2_data_survives_upgrade_that_refuses_older_runtime_version() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("compatibility.sqlite");
    let conn = open_initialized(&path).unwrap();
    conn.execute(
        "INSERT INTO codex_delivery_outbox (delivery_id,job_id,target_thread_id,turn_id,channel_id,content,created_at,updated_at)
         VALUES ('delivery-a','job-a','thread-a','turn-a',7,'keep-me',1,1)", [],
    ).unwrap();
    drop(conn);
    admit_prompt_intake(
        &path,
        NewPromptIntake {
            job_id: "intake-a",
            target_thread_id: "thread-a",
            channel_id: 7,
            owner_user_id: Some(8),
            discord_message_id: Some(9),
            raw_prompt: "keep enriched raw prompt",
            auto_queue_when_busy: true,
            require_current_mirror: true,
            created_at: 1.0,
        },
    )
    .unwrap();

    // Version 3 is deliberately above the old runtime's supported version.
    // Raw inspection does not represent permission to run the old initializer.
    let legacy = Connection::open(&path).unwrap();
    assert_eq!(
        legacy
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        cdr_store::schema::LATEST_STORE_SCHEMA_VERSION
    );
    legacy
        .execute(
            "INSERT INTO discord_processed_messages VALUES (77,12.5)",
            [],
        )
        .unwrap();
    drop(legacy);
    let conn = open_initialized(&path).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM discord_processed_messages WHERE message_id=77",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT content FROM codex_delivery_outbox WHERE delivery_id='delivery-a'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "keep-me"
    );
    let intake = get_prompt_intake(&path, "intake-a").unwrap().unwrap();
    assert_eq!(intake.raw_prompt, "keep enriched raw prompt");
    assert_eq!(intake.discord_message_id, Some(9));
}

#[test]
fn rust_replay_barriers_survive_snapshot_and_reopen_at_far_future_time() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("replay.sqlite");
    assert!(claim(&path, 801, 1.0).unwrap());
    assert!(claim(&path, 802, 2.0).unwrap());
    mark(&path, 802, 3.0).unwrap();
    let snapshot = cdr_store::backup::snapshot(&path).unwrap();
    for path in [&path, &snapshot] {
        assert!(!claim(path, 801, FAR_FUTURE).unwrap());
        assert!(!claim(path, 802, FAR_FUTURE).unwrap());
        // Same INSERT OR IGNORE used by the legacy writer must also reject replay.
        let legacy = Connection::open(path).unwrap();
        assert_eq!(
            legacy
                .execute(
                    "INSERT OR IGNORE INTO discord_processed_messages VALUES (801,10000000001)",
                    []
                )
                .unwrap(),
            0
        );
        assert_eq!(
            legacy
                .execute(
                    "INSERT OR IGNORE INTO discord_processed_messages VALUES (802,10000000001)",
                    []
                )
                .unwrap(),
            0
        );
    }
}

#[test]
fn legacy_claimed_and_marked_rows_block_rust_reclaim_at_far_future_time() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("legacy-replay.sqlite");
    let legacy = legacy_store(&path);
    legacy
        .execute_batch(
            "INSERT OR IGNORE INTO discord_processed_messages VALUES (811,1.0);
        INSERT OR IGNORE INTO discord_processed_messages VALUES (812,2.0);
        INSERT OR REPLACE INTO discord_processed_messages VALUES (812,3.0);",
        )
        .unwrap();
    drop(legacy);
    assert!(!claim(&path, 811, FAR_FUTURE).unwrap());
    assert!(!claim(&path, 812, FAR_FUTURE).unwrap());
}

#[test]
fn legacy_mark_only_row_keeps_exact_schema_and_blocks_rust_reclaim() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("mark-only.sqlite");
    let conn = legacy_store(&path);
    conn.execute(
        "INSERT OR REPLACE INTO discord_processed_messages VALUES (821,12.5)",
        [],
    )
    .unwrap();
    drop(conn);
    let connection = open_initialized(&path).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        cdr_store::schema::LATEST_STORE_SCHEMA_VERSION
    );
    let mut statement = connection
        .prepare("PRAGMA table_info(discord_processed_messages)")
        .unwrap();
    let columns = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        columns,
        vec![
            (0, "message_id".into(), "INTEGER".into(), 0, None, 1),
            (1, "seen_at".into(), "REAL".into(), 1, None, 0),
        ]
    );
    let rows = connection
        .prepare("SELECT message_id,seen_at FROM discord_processed_messages ORDER BY message_id")
        .unwrap()
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows, vec![(821, 12.5)]);
    drop(statement);
    drop(connection);
    assert!(!claim(&path, 821, FAR_FUTURE).unwrap());
}

#[test]
fn empty_legacy_v2_store_can_be_reopened_and_extended_idempotently() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("empty.sqlite");
    drop(legacy_store(&path));
    for _ in 0..2 {
        let conn = open_initialized(&path).unwrap();
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            cdr_store::schema::LATEST_STORE_SCHEMA_VERSION
        );
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='discord_processed_messages'",[],|row|row.get::<_,i64>(0)).unwrap(),1);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM discord_processed_messages",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
    }
}
