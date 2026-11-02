use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, params};

use super::StoredPromptIntake;
use crate::{Result, StoreError};

const TABLE: &str = "codex_prompt_intakes";
const MESSAGE_INDEX: &str = "codex_prompt_intakes_message_id";
const TARGET_READY_INDEX: &str = "codex_prompt_intakes_target_ready";

const COLUMNS: &str = "job_id, target_thread_id, channel_id, owner_user_id, \
    discord_message_id, raw_prompt, auto_queue_when_busy, require_current_mirror, \
    attempt_count, last_error, retry_after, claim_token, claim_expires_at, created_at, updated_at";

const REQUIRED_COLUMNS: [&str; 15] = [
    "job_id",
    "target_thread_id",
    "channel_id",
    "owner_user_id",
    "discord_message_id",
    "raw_prompt",
    "auto_queue_when_busy",
    "require_current_mirror",
    "attempt_count",
    "last_error",
    "retry_after",
    "claim_token",
    "claim_expires_at",
    "created_at",
    "updated_at",
];

pub(super) fn ensure_schema(connection: &Connection) -> Result<()> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS codex_prompt_intakes (\
            job_id TEXT PRIMARY KEY, target_thread_id TEXT NOT NULL, \
            channel_id INTEGER NOT NULL, owner_user_id INTEGER, discord_message_id INTEGER, \
            raw_prompt TEXT NOT NULL, auto_queue_when_busy INTEGER NOT NULL, \
            require_current_mirror INTEGER NOT NULL, attempt_count INTEGER NOT NULL DEFAULT 0, \
            last_error TEXT NOT NULL DEFAULT '', retry_after REAL NOT NULL DEFAULT 0, \
            claim_token TEXT, claim_expires_at REAL NOT NULL DEFAULT 0, \
            created_at REAL NOT NULL, updated_at REAL NOT NULL, \
            CHECK (auto_queue_when_busy IN (0, 1)), \
            CHECK (require_current_mirror IN (0, 1))\
        )",
        [],
    )?;
    add_missing_columns(connection)?;
    connection.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS codex_prompt_intakes_message_id \
         ON codex_prompt_intakes(discord_message_id) WHERE discord_message_id IS NOT NULL",
        [],
    )?;
    connection.execute(
        "CREATE INDEX IF NOT EXISTS codex_prompt_intakes_target_ready \
         ON codex_prompt_intakes(target_thread_id, retry_after, claim_expires_at, created_at, job_id)",
        [],
    )?;
    Ok(())
}

pub(super) fn schema_current(connection: &Connection) -> Result<bool> {
    let objects: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE \
         (type = 'table' AND name = ?) OR (type = 'index' AND name IN (?, ?))",
        params![TABLE, MESSAGE_INDEX, TARGET_READY_INDEX],
        |row| row.get(0),
    )?;
    if objects != 3 {
        return Ok(false);
    }
    let columns = columns(connection)?;
    Ok(REQUIRED_COLUMNS
        .iter()
        .all(|column| columns.contains(*column)))
}

pub(super) fn by_job(connection: &Connection, job_id: &str) -> Result<Option<StoredPromptIntake>> {
    select_one(
        connection,
        &format!("SELECT {COLUMNS} FROM {TABLE} WHERE job_id = ?"),
        job_id,
    )
}

pub(super) fn by_message(
    connection: &Connection,
    message_id: i64,
) -> Result<Option<StoredPromptIntake>> {
    select_one(
        connection,
        &format!("SELECT {COLUMNS} FROM {TABLE} WHERE discord_message_id = ?"),
        &message_id,
    )
}

pub(super) fn list(
    connection: &Connection,
    ready_at: Option<f64>,
) -> Result<Vec<StoredPromptIntake>> {
    let (sql, parameter) = ready_at.map_or_else(
        || {
            (
                format!("SELECT {COLUMNS} FROM cdr_live_codex_prompt_intakes ORDER BY created_at, job_id"),
                None,
            )
        },
        |now| {
            (
                format!(
                    "SELECT {COLUMNS} FROM cdr_live_codex_prompt_intakes WHERE retry_after <= ? \
                     AND claim_expires_at <= ? ORDER BY created_at, job_id"
                ),
                Some(now),
            )
        },
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = match parameter {
        Some(now) => statement.query_map(params![now, now], from_row)?,
        None => statement.query_map([], from_row)?,
    };
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn add_missing_columns(connection: &Connection) -> Result<()> {
    let columns = columns(connection)?;
    if !columns.contains("job_id") {
        return Err(StoreError::Integrity(
            "codex_prompt_intakes is missing its job_id primary identity".into(),
        ));
    }
    for (name, definition) in [
        ("target_thread_id", "TEXT NOT NULL DEFAULT ''"),
        ("channel_id", "INTEGER NOT NULL DEFAULT 0"),
        ("owner_user_id", "INTEGER"),
        ("discord_message_id", "INTEGER"),
        ("raw_prompt", "TEXT NOT NULL DEFAULT ''"),
        ("auto_queue_when_busy", "INTEGER NOT NULL DEFAULT 0"),
        ("require_current_mirror", "INTEGER NOT NULL DEFAULT 0"),
        ("attempt_count", "INTEGER NOT NULL DEFAULT 0"),
        ("last_error", "TEXT NOT NULL DEFAULT ''"),
        ("retry_after", "REAL NOT NULL DEFAULT 0"),
        ("claim_token", "TEXT"),
        ("claim_expires_at", "REAL NOT NULL DEFAULT 0"),
        ("created_at", "REAL NOT NULL DEFAULT 0"),
        ("updated_at", "REAL NOT NULL DEFAULT 0"),
    ] {
        if !columns.contains(name) {
            connection.execute(
                &format!("ALTER TABLE {TABLE} ADD COLUMN {name} {definition}"),
                [],
            )?;
        }
    }
    Ok(())
}

fn columns(connection: &Connection) -> Result<BTreeSet<String>> {
    let mut statement = connection.prepare("PRAGMA table_info(codex_prompt_intakes)")?;
    Ok(statement
        .query_map([], |row| row.get(1))?
        .collect::<rusqlite::Result<_>>()?)
}

fn select_one<T: rusqlite::ToSql + ?Sized>(
    connection: &Connection,
    sql: &str,
    value: &T,
) -> Result<Option<StoredPromptIntake>> {
    Ok(connection.query_row(sql, [value], from_row).optional()?)
}

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredPromptIntake> {
    Ok(StoredPromptIntake {
        job_id: row.get(0)?,
        target_thread_id: row.get(1)?,
        channel_id: row.get(2)?,
        owner_user_id: row.get(3)?,
        discord_message_id: row.get(4)?,
        raw_prompt: row.get(5)?,
        auto_queue_when_busy: row.get::<_, i64>(6)? != 0,
        require_current_mirror: row.get::<_, i64>(7)? != 0,
        attempt_count: row.get(8)?,
        last_error: row.get(9)?,
        retry_after: row.get(10)?,
        claim_token: row.get(11)?,
        claim_expires_at: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}
