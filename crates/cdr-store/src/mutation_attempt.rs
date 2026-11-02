//! Pre-dispatch evidence, never a lease or permission to replay.
pub mod response;
pub mod resume_recovery;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Result, StoreError, schema};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

const MAX_PREPARED: i64 = 1024;
const MAX_CONFIRMED: i64 = 256;

pub(crate) fn migrate_schema(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS codex_mutation_runtime(
        singleton INTEGER PRIMARY KEY CHECK(singleton=1), runtime_id TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS codex_mutation_attempts(
        sequence INTEGER PRIMARY KEY AUTOINCREMENT, attempt_id TEXT NOT NULL UNIQUE,
        runtime_id TEXT NOT NULL, owner_id TEXT NOT NULL, generation INTEGER NOT NULL,
        wire_id TEXT NOT NULL, method TEXT NOT NULL, target_thread_id TEXT,
        scoped INTEGER NOT NULL CHECK(scoped IN (0,1)),
        request_sha256 TEXT NOT NULL,
        state TEXT NOT NULL CHECK(state IN ('prepared','reply_ok','reply_error','not_sent')),
        created_at REAL NOT NULL, updated_at REAL NOT NULL,
        CHECK(scoped=0 OR length(target_thread_id)>0));
        CREATE UNIQUE INDEX IF NOT EXISTS codex_mutation_prepared_target
        ON codex_mutation_attempts(target_thread_id) WHERE scoped=1 AND state='prepared';
        CREATE INDEX IF NOT EXISTS codex_mutation_prepared
        ON codex_mutation_attempts(state,scoped,target_thread_id);",
    )?;
    resume_recovery::migrate_schema(db)
}

pub(crate) fn schema_current(db: &Connection) -> Result<bool> {
    Ok(db.query_row("SELECT
        EXISTS(SELECT 1 FROM pragma_table_info('codex_mutation_attempts') WHERE name='request_sha256')
        AND EXISTS(SELECT 1 FROM pragma_table_info('codex_mutation_runtime') WHERE name='runtime_id')
        AND EXISTS(SELECT 1 FROM sqlite_schema WHERE name='codex_mutation_prepared_target')
        AND EXISTS(SELECT 1 FROM sqlite_schema WHERE name='cdr_resume_restart_recoveries')",
        [], |r| r.get(0))?)
}

fn refused(message: impl Into<String>) -> StoreError {
    StoreError::Integrity(message.into())
}

fn existing(path: &Path) -> Result<Connection> {
    // Dispatch never creates or silently migrates an absent/replaced database.
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(schema::STORE_BUSY_TIMEOUT)?;
    Ok(db)
}

fn owner_is_current(db: &Connection, runtime: &str) -> Result<()> {
    let current: Option<String> = db
        .query_row(
            "SELECT runtime_id FROM codex_mutation_runtime WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if current.as_deref() != Some(runtime) {
        return Err(refused(
            "mutation journal runtime changed or is missing; dispatch held",
        ));
    }
    Ok(())
}

pub fn activate(path: &Path, runtime: &str) -> Result<()> {
    if runtime.trim().is_empty() {
        return Err(refused("empty mutation runtime"));
    }
    let db = schema::open_initialized(path)?;
    db.execute(
        "INSERT INTO codex_mutation_runtime(singleton,runtime_id) VALUES(1,?)
        ON CONFLICT(singleton) DO UPDATE SET runtime_id=excluded.runtime_id",
        [runtime],
    )?;
    // Never delete prepared attempts, even when their process/owner has changed.
    Ok(())
}

fn unblocked(db: &Connection, target: Option<&str>) -> Result<()> {
    let held: Option<(String, String)> = db
        .query_row(
            "SELECT attempt_id,method FROM codex_mutation_attempts
        WHERE state='prepared' AND (?1 IS NULL OR scoped=0 OR target_thread_id=?1)
        ORDER BY sequence LIMIT 1",
            [target],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((attempt, method)) = held {
        return Err(refused(format!(
            "unresolved mutation {attempt} ({method}); dispatch held without replay"
        )));
    }
    Ok(())
}

pub fn check(path: &Path, runtime: &str, target: Option<&str>) -> Result<()> {
    let db = existing(path)?;
    owner_is_current(&db, runtime)?;
    unblocked(&db, target)
}

pub struct NewAttempt<'a> {
    pub runtime_id: &'a str,
    pub owner_id: &'a str,
    pub generation: i64,
    pub attempt_id: &'a str,
    pub wire_id: &'a str,
    pub method: &'a str,
    pub target_thread_id: Option<&'a str>,
    pub scoped: bool,
    pub payload: &'a Value,
}

pub fn begin(path: &Path, attempt: &NewAttempt<'_>) -> Result<()> {
    begin_checked(path, attempt, |_| Ok(()))
}

/// Serialize a durable authority check with the final writer claim.
///
/// The caller must carry the original admission identity across all waits. The
/// check must use this transaction's connection, not a separate connection or a
/// newly captured revision. It runs again after INSERT to reject changed
/// authority before commit. A committed claim remains evidence, not replay
/// permission, if stop/cancellation wins after this function returns.
pub fn begin_checked(
    path: &Path,
    attempt: &NewAttempt<'_>,
    check: impl Fn(&Connection) -> Result<()>,
) -> Result<()> {
    if [
        attempt.runtime_id,
        attempt.owner_id,
        attempt.attempt_id,
        attempt.wire_id,
        attempt.method,
    ]
    .iter()
    .any(|v| v.trim().is_empty())
        || attempt.generation < 1
        || (attempt.scoped && attempt.target_thread_id.is_none_or(|t| t.trim().is_empty()))
    {
        return Err(refused("invalid mutation attempt identity"));
    }
    let mut db = existing(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    owner_is_current(&tx, attempt.runtime_id)?;
    check(&tx)?;
    unblocked(
        &tx,
        if attempt.scoped {
            attempt.target_thread_id
        } else {
            None
        },
    )?;
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM cdr_live_codex_mutation_attempts WHERE state='prepared'",
        [],
        |r| r.get(0),
    )?;
    if count >= MAX_PREPARED {
        return Err(refused(
            "unresolved mutation capacity reached; no eviction or dispatch",
        ));
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
    let hash = hex::encode(Sha256::digest(serde_json::to_vec(attempt.payload)?));
    if tx.execute(
        "INSERT INTO codex_mutation_attempts(attempt_id,runtime_id,owner_id,generation,
        wire_id,method,target_thread_id,scoped,request_sha256,state,created_at,updated_at)
        VALUES(?,?,?,?,?,?,?,?,?,'prepared',?,?)",
        params![
            attempt.attempt_id,
            attempt.runtime_id,
            attempt.owner_id,
            attempt.generation,
            attempt.wire_id,
            attempt.method,
            attempt.target_thread_id,
            attempt.scoped,
            hash,
            now,
            now
        ],
    )? != 1
    {
        return Err(refused("mutation intent insert did not claim a row"));
    }
    owner_is_current(&tx, attempt.runtime_id)?;
    let retained: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM codex_mutation_attempts
        WHERE attempt_id=? AND runtime_id=? AND owner_id=? AND generation=? AND wire_id=?
        AND method=? AND target_thread_id IS ? AND scoped=? AND request_sha256=? AND state='prepared')",
        params![attempt.attempt_id,attempt.runtime_id,attempt.owner_id,attempt.generation,
            attempt.wire_id,attempt.method,attempt.target_thread_id,attempt.scoped,hash], |r| r.get(0))?;
    if !retained {
        return Err(refused("mutation intent changed before commit"));
    }
    check(&tx)?;
    tx.commit()?;
    Ok(())
}

pub struct Completion<'a> {
    pub runtime_id: &'a str,
    pub owner_id: &'a str,
    pub generation: i64,
    pub attempt_id: &'a str,
    pub wire_id: &'a str,
    pub outcome: &'a str,
}

pub fn finish(path: &Path, completion: &Completion<'_>) -> Result<()> {
    if !matches!(completion.outcome, "not_sent" | "reply_ok" | "reply_error") {
        return Err(refused("unknown outcome cannot settle a mutation attempt"));
    }
    let mut db = existing(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    owner_is_current(&tx, completion.runtime_id)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
    if tx.execute("UPDATE codex_mutation_attempts SET state=?,updated_at=?
        WHERE attempt_id=? AND runtime_id=? AND owner_id=? AND generation=? AND wire_id=? AND state='prepared'",
        params![completion.outcome,now,completion.attempt_id,completion.runtime_id,
            completion.owner_id,completion.generation,completion.wire_id])? != 1
    { return Err(refused("mutation completion lost its exact owner/request occurrence")); }
    let retained: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM codex_mutation_attempts
        WHERE attempt_id=? AND runtime_id=? AND owner_id=? AND generation=? AND wire_id=? AND state=?)",
        params![completion.attempt_id,completion.runtime_id,completion.owner_id,
            completion.generation,completion.wire_id,completion.outcome], |r| r.get(0))?;
    owner_is_current(&tx, completion.runtime_id)?;
    if !retained {
        return Err(refused("mutation completion changed before commit"));
    }
    // Prune only definite history. Never expire an unknown attempt to make room.
    tx.execute(
        "DELETE FROM codex_mutation_attempts WHERE sequence IN
        (SELECT sequence FROM codex_mutation_attempts WHERE state!='prepared'
        ORDER BY updated_at DESC,sequence DESC LIMIT -1 OFFSET ?)",
        [MAX_CONFIRMED],
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests;
