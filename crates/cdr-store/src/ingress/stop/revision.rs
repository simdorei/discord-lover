//! Monotonic stop ordering. A revision is evidence, never permission to replay.
use super::{StopReceipt, StopScope};

mod archive;

/// The Archive adapter alone may derive a verified subtree from the first origin.
/// Ordinary requests keep the exact two-field, single-target contract.
pub fn validate_request_in(
    db: &Connection,
    method: &str,
    target: Option<&str>,
    origin: Option<&Value>,
) -> Result<()> {
    if let Some(origin) = origin.filter(|value| value.get("ingressId").is_some()) {
        return validate_ingress_origin_in(db, method, target, origin);
    }
    if let Some(origin) = origin.filter(|value| value.get("archiveTargets").is_some()) {
        archive::validate_in(db, method, target, origin)
    } else {
        validate_in(db, target, origin)
    }
}

// This runs inside the actual writer's mutation transaction, both before and
// after its intent INSERT. Restoring a room never revives an old admission.
fn validate_ingress_origin_in(
    db: &Connection,
    method: &str,
    target: Option<&str>,
    envelope: &Value,
) -> Result<()> {
    if method == "thread/archive" || envelope.as_object().is_none_or(|value| value.len() != 2) {
        return Err(refused());
    }
    let key = envelope["ingressId"].as_str().ok_or_else(refused)?;
    let record = crate::ingress::read::get_in(db, key)?.ok_or_else(refused)?;
    let original = origin_for_ingress(&record)?;
    if record.target_thread_id.as_deref() != target
        || envelope.get("stopOrigin") != Some(original.as_ref().unwrap_or(&Value::Null))
    {
        return Err(refused());
    }
    if crate::archived_cleanup::record_retired_in(db, "discord_ingress_journal", key)? {
        return Err(StoreError::Integrity(
            "original ingress was retired by archived cleanup; no dispatch".into(),
        ));
    }
    validate_in(db, target, original.as_ref())
}
use crate::{Result, StoreError};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::Path;

fn refused() -> StoreError {
    StoreError::Integrity(
        "original RPC predates stop or stop revision evidence differs; no dispatch".into(),
    )
}

pub(crate) fn migrate_schema(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS cdr_stop_clock(
            singleton INTEGER PRIMARY KEY CHECK(singleton=1),
            revision INTEGER NOT NULL CHECK(revision>=0));
         INSERT OR IGNORE INTO cdr_stop_clock VALUES(1,0);
         CREATE TABLE IF NOT EXISTS cdr_stop_revisions(
            target_thread_id TEXT PRIMARY KEY, revision INTEGER NOT NULL,
            operation_id TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS cdr_stop_revision_receipts(
            operation_id TEXT PRIMARY KEY, target_thread_id TEXT NOT NULL,
            revision INTEGER NOT NULL UNIQUE CHECK(revision>0), scope_json TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS cdr_stop_revision_target
            ON cdr_stop_revision_receipts(target_thread_id,revision);",
    )?;
    Ok(())
}

pub(crate) fn schema_current(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT (SELECT group_concat(name,',') FROM pragma_table_info('cdr_stop_clock'))='singleton,revision'
         AND (SELECT group_concat(name,',') FROM pragma_table_info('cdr_stop_revisions'))='target_thread_id,revision,operation_id'
         AND (SELECT group_concat(name,',') FROM pragma_table_info('cdr_stop_revision_receipts'))='operation_id,target_thread_id,revision,scope_json'
         AND EXISTS(SELECT 1 FROM sqlite_schema WHERE name='cdr_stop_revision_target')",
        [], |row| Ok(row.get::<_,Option<bool>>(0)?.unwrap_or(false)),
    )?)
}

fn current_in(db: &Connection) -> Result<i64> {
    let (count, current, maximum): (i64, i64, i64) = db.query_row(
        "SELECT count(*),COALESCE(max(revision),-1),
            (SELECT COALESCE(max(revision),0) FROM cdr_stop_revision_receipts)
         FROM cdr_stop_clock WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if count != 1 || current < 0 || current != maximum {
        return Err(refused());
    }
    Ok(current)
}

/// Read the immutable latest accepted scope in the ACK writer transaction.
pub(super) fn latest_scope_in(db: &Connection, target: &str) -> Result<Option<(String, String)>> {
    if db.is_autocommit() {
        return Err(StoreError::ActiveTransaction);
    }
    let _ = current_in(db)?;
    let _ = target_in(db, target)?;
    Ok(db
        .query_row(
            "SELECT operation_id,scope_json FROM cdr_stop_revision_receipts
         WHERE target_thread_id=? ORDER BY revision DESC LIMIT 1",
            [target],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

fn target_in(db: &Connection, target: &str) -> Result<i64> {
    let latest: Option<(i64, String)> = db
        .query_row(
            "SELECT revision,operation_id FROM cdr_stop_revisions WHERE target_thread_id=?",
            [target],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let history: Option<(i64, String)> = db
        .query_row(
            "SELECT revision,operation_id FROM cdr_stop_revision_receipts
         WHERE target_thread_id=? ORDER BY revision DESC LIMIT 1",
            [target],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if latest != history {
        return Err(refused());
    }
    Ok(latest.map_or(0, |(revision, _)| revision))
}

/// Capture in the same read/write transaction as the original admission.
pub(crate) fn capture_in(db: &Connection, target: Option<&str>) -> Result<Value> {
    if db.is_autocommit() {
        return Err(StoreError::ActiveTransaction);
    }
    let revision = current_in(db)?;
    if let Some(target) = target {
        let _ = target_in(db, target)?;
    }
    Ok(json!({"target":target,"stopRevision":revision}))
}

/// Read original server-owned metadata only. Missing legacy evidence is revision
/// zero at dispatch, never a reason to capture the current clock.
pub fn origin_for_ingress(record: &crate::ingress::StoredIngress) -> Result<Option<Value>> {
    let Some(origin) = record.payload.get("stop_origin") else {
        return Ok(None);
    };
    if origin.as_object().is_none_or(|value| value.len() != 2)
        || origin.get("target") != Some(&json!(record.target_thread_id.as_deref()))
        || origin
            .get("stopRevision")
            .and_then(Value::as_i64)
            .is_none_or(|revision| revision < 0)
    {
        return Err(refused());
    }
    Ok(Some(origin.clone()))
}

/// Capture before any async preparation or pipe wait. Reads do not migrate.
pub fn capture(path: &Path, target: Option<&str>) -> Result<Value> {
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_millis(500))?;
    let tx = db.transaction()?;
    let origin = capture_in(&tx, target)?;
    tx.commit()?;
    Ok(origin)
}

/// Same connection as mutation intent; called before and after its INSERT.
/// No origin is a legacy pre-stop request, not permission to borrow current state.
pub fn validate_in(db: &Connection, target: Option<&str>, origin: Option<&Value>) -> Result<()> {
    let revision = if let Some(origin) = origin {
        if origin.as_object().is_none_or(|value| value.len() != 2)
            || origin.get("target") != Some(&json!(target))
        {
            return Err(refused());
        }
        origin
            .get("stopRevision")
            .and_then(Value::as_i64)
            .ok_or_else(refused)?
    } else {
        0
    };
    if revision < 0 || revision > current_in(db)? {
        return Err(refused());
    }
    if let Some(target) = target
        && target_in(db, target)? > revision
    {
        return Err(refused());
    }
    Ok(())
}

/// Explicit recovery cancellation revokes earlier ordinary RPC admissions.
/// This is a lifecycle ordering receipt, not a Stop command or interrupt grant.
pub(crate) fn record_recovery_in(
    db: &Connection,
    scope: StopScope<'_>,
    jobs: &[String],
    cancelled_at: f64,
) -> Result<()> {
    if db.is_autocommit() {
        return Err(StoreError::ActiveTransaction);
    }
    let current = current_in(db)?;
    let _ = target_in(db, scope.target)?;
    let revision = current.checked_add(1).ok_or_else(refused)?;
    let record = Advance {
        target: scope.target.into(),
        revision,
        operation: format!("recovery-cancel:{}", uuid::Uuid::new_v4()),
        scope_json: json!({
            "kind": "recovery-cancellation", "target": scope.target,
            "channel": scope.channel, "owner": scope.owner,
            "jobs": jobs, "cancelled_at": cancelled_at,
        })
        .to_string(),
    };
    if db.execute(
        "UPDATE cdr_stop_clock SET revision=? WHERE singleton=1 AND revision=?",
        params![revision, current],
    )? != 1
        || db.execute(
            "INSERT INTO cdr_stop_revision_receipts
         (operation_id,target_thread_id,revision,scope_json) VALUES(?,?,?,?)",
            params![record.operation, record.target, revision, record.scope_json],
        )? != 1
        || db.execute(
            "INSERT INTO cdr_stop_revisions(target_thread_id,revision,operation_id)
         VALUES(?,?,?) ON CONFLICT(target_thread_id) DO UPDATE
         SET revision=excluded.revision,operation_id=excluded.operation_id",
            params![record.target, revision, record.operation],
        )? != 1
    {
        return Err(refused());
    }
    verify_in(db, &record)
}

pub(super) struct Advance {
    target: String,
    revision: i64,
    operation: String,
    scope_json: String,
}

pub(super) fn advance_in(
    db: &Connection,
    scope: StopScope<'_>,
    binding: &Value,
    receipt: &StopReceipt,
    operation: &str,
) -> Result<Advance> {
    if db.is_autocommit() {
        return Err(StoreError::ActiveTransaction);
    }
    if operation.trim().is_empty() {
        return Err(refused());
    }
    let current = current_in(db)?;
    let _ = target_in(db, scope.target)?;
    let revision = current.checked_add(1).ok_or_else(refused)?;
    let record = Advance {
        target: scope.target.into(),
        revision,
        operation: operation.into(),
        scope_json: json!({"target":scope.target,"channel":scope.channel,"owner":scope.owner,
            "hadPreparing":had_preparing_in(db,scope.target)?,
            "binding":binding,"jobs":receipt.jobs,"ingresses":receipt.ingresses})
        .to_string(),
    };
    if db.execute(
        "UPDATE cdr_stop_clock SET revision=? WHERE singleton=1 AND revision=?",
        params![revision, current],
    )? != 1
        || db.execute(
            "INSERT INTO cdr_stop_revision_receipts
            (operation_id,target_thread_id,revision,scope_json) VALUES(?,?,?,?)",
            params![operation, scope.target, revision, record.scope_json],
        )? != 1
        || db.execute(
            "INSERT INTO cdr_stop_revisions(target_thread_id,revision,operation_id)
            VALUES(?,?,?) ON CONFLICT(target_thread_id) DO UPDATE
            SET revision=excluded.revision,operation_id=excluded.operation_id",
            params![scope.target, revision, operation],
        )? != 1
    {
        return Err(refused());
    }
    verify_in(db, &record)?;
    Ok(record)
}

fn had_preparing_in(db: &Connection, target: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM codex_prompt_intakes WHERE target_thread_id=?)",
        [target],
        |row| row.get(0),
    )?)
}

pub(super) fn verify_in(db: &Connection, record: &Advance) -> Result<()> {
    let retained: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM cdr_stop_revision_receipts WHERE operation_id=?
         AND target_thread_id=? AND revision=? AND scope_json=?)",
        params![
            record.operation,
            record.target,
            record.revision,
            record.scope_json
        ],
        |row| row.get(0),
    )?;
    if current_in(db)? != record.revision
        || target_in(db, &record.target)? != record.revision
        || !retained
    {
        return Err(refused());
    }
    Ok(())
}
