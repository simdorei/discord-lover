//! Progress messages are saved before waiting for the first reply or Discord I/O.
use crate::{Result, schema::open_initialized};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct PendingCommentary {
    pub sequence: i64,
    pub job_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub channel_id: i64,
    pub text: String,
}

pub(crate) fn migrate_schema(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS codex_commentary_outbox (
        sequence INTEGER PRIMARY KEY AUTOINCREMENT, delivery_key TEXT NOT NULL UNIQUE,
        job_id TEXT NOT NULL, target_thread_id TEXT NOT NULL, turn_id TEXT NOT NULL,
        channel_id INTEGER NOT NULL, text TEXT NOT NULL);",
    )?;
    Ok(())
}

pub(crate) fn schema_current(c: &Connection) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table'
        AND name='codex_commentary_outbox')",
        [],
        |r| r.get(0),
    )?)
}

pub fn stage(
    path: &Path,
    thread: &str,
    turn: &str,
    text: &str,
) -> Result<Option<PendingCommentary>> {
    let mut c = open_initialized(path)?;
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let job = tx
        .query_row(
            "SELECT job_id,channel_id FROM codex_turn_queue WHERE
        target_thread_id=? AND turn_id=? AND state='running'",
            params![thread, turn],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    let Some((job, channel)) = job else {
        return Ok(None);
    };
    crate::dead_generation::ensure_target_available(&tx, thread)?;
    let text = text.trim();
    let key = hex::encode(Sha256::digest(serde_json::to_vec(&(thread, turn, text))?));
    tx.execute(
        "INSERT OR IGNORE INTO codex_commentary_outbox
        (delivery_key,job_id,target_thread_id,turn_id,channel_id,text) VALUES (?,?,?,?,?,?)",
        params![key, job, thread, turn, channel, text],
    )?;
    let item = tx.query_row(
        "SELECT sequence,job_id,target_thread_id,turn_id,channel_id,text
        FROM codex_commentary_outbox WHERE delivery_key=?",
        [key],
        read,
    )?;
    tx.commit()?;
    Ok(Some(item))
}

pub fn pending(path: &Path) -> Result<Vec<PendingCommentary>> {
    let c = open_initialized(path)?;
    Ok(c.prepare(
        "SELECT sequence,job_id,target_thread_id,turn_id,channel_id,text
        FROM cdr_live_codex_commentary_outbox ORDER BY sequence",
    )?
    .query_map([], read)?
    .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// `before=None` checks all progress, as required before a final reply.
pub fn has_pending(path: &Path, job: &str, before: Option<i64>) -> Result<bool> {
    let connection = open_initialized(path)?;
    has_pending_in(&connection, job, before)
}

pub(crate) fn has_pending_in(
    connection: &Connection,
    job: &str,
    before: Option<i64>,
) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM codex_commentary_outbox
        WHERE job_id=?1 AND (?2 IS NULL OR sequence<?2))",
        params![job, before],
        |r| r.get(0),
    )?)
}

pub fn complete(path: &Path, sequence: i64) -> Result<()> {
    open_initialized(path)?.execute(
        "DELETE FROM codex_commentary_outbox WHERE sequence=?",
        [sequence],
    )?;
    Ok(())
}

fn read(row: &Row<'_>) -> rusqlite::Result<PendingCommentary> {
    Ok(PendingCommentary {
        sequence: row.get(0)?,
        job_id: row.get(1)?,
        thread_id: row.get(2)?,
        turn_id: row.get(3)?,
        channel_id: row.get(4)?,
        text: row.get(5)?,
    })
}
