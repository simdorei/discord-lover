//! Retire only old, idle thread-loading attempts after a cold runtime restart.
//! The original result remains unknown in the audit; this never settles a turn/start.
use super::{existing, owner_is_current, refused};
use crate::{Result, execution_hold};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn migrate_schema(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS cdr_resume_restart_recoveries(
        attempt_id TEXT PRIMARY KEY, original_json TEXT NOT NULL,
        proof_json TEXT NOT NULL, runtime_id TEXT NOT NULL, recovered_at REAL NOT NULL);",
    )?;
    Ok(())
}

const RECORD: &str = "SELECT json_object(
    'sequence',sequence,'attempt_id',attempt_id,'runtime_id',runtime_id,
    'owner_id',owner_id,'generation',generation,'wire_id',wire_id,'method',method,
    'target_thread_id',target_thread_id,'scoped',scoped,'request_sha256',request_sha256,
    'state',state,'created_at',created_at,'updated_at',updated_at)
    FROM codex_mutation_attempts WHERE attempt_id=?";

#[derive(Clone, Debug)]
pub struct Candidate {
    pub attempt_id: String,
    pub target: String,
    original: String,
}

pub fn candidates(path: &Path, runtime: &str) -> Result<Vec<Candidate>> {
    let db = existing(path)?;
    owner_is_current(&db, runtime)?;
    let rows = db
        .prepare(
            "SELECT attempt_id,target_thread_id FROM cdr_live_codex_mutation_attempts
        WHERE state='prepared' AND method='thread/resume' AND scoped=1 AND runtime_id<>?
        ORDER BY sequence LIMIT 64",
        )?
        .query_map([runtime], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(attempt_id, target)| {
            let original = db.query_row(RECORD, [&attempt_id], |r| r.get(0))?;
            Ok(Candidate {
                attempt_id,
                target,
                original,
            })
        })
        .collect()
}

/// No missing/unknown status, active turn or incomplete empty page proves idle.
#[must_use]
pub fn is_idle_observation(target: &str, metadata: &Value, latest: &Value) -> bool {
    if metadata.pointer("/thread/id").and_then(Value::as_str) != Some(target)
        || !matches!(
            metadata
                .pointer("/thread/status/type")
                .and_then(Value::as_str),
            Some("idle" | "notLoaded")
        )
        || latest
            .get("truncated")
            .is_some_and(|v| v != &Value::Bool(false))
    {
        return false;
    }
    let Some(turns) = latest.get("data").and_then(Value::as_array) else {
        return false;
    };
    match turns.as_slice() {
        [] => matches!(latest.get("nextCursor"), Some(Value::Null)),
        [turn] => {
            turn.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
                && matches!(
                    turn.get("status").and_then(Value::as_str),
                    Some("completed" | "interrupted" | "failed")
                )
        }
        _ => false,
    }
}

fn matching_preflight(error: &str, attempt: &str) -> bool {
    let Some(error) = error.strip_prefix(execution_hold::PREFIX) else {
        return false;
    };
    (error.starts_with("app-server mutation thread/resume outcome remains unknown:")
        && error.ends_with(&format!(
            "; durable attempt {attempt} retained; no automatic replay"
        )))
        || error
            == format!(
                "mutation execution held: SQLite integrity check failed: unresolved mutation {attempt} (thread/resume); dispatch held without replay"
            )
}

/// Called before workers/admission, with observations from the new resident.
/// A later resume still has to acquire Codex's native writer and pass all normal gates.
pub fn recover(
    path: &Path,
    runtime: &str,
    resident: (&str, i64),
    candidate: &Candidate,
    metadata: &Value,
    latest: &Value,
) -> Result<usize> {
    if resident.0.is_empty()
        || resident.1 < 1
        || !is_idle_observation(&candidate.target, metadata, latest)
    {
        return Err(refused("resume restart recovery lacks an idle observation"));
    }
    let old: Value = serde_json::from_str(&candidate.original)?;
    if old["runtime_id"] == runtime
        || old["owner_id"] == resident.0
        || old["method"] != "thread/resume"
        || old["state"] != "prepared"
        || old["scoped"] != 1
        || old["target_thread_id"] != candidate.target
        || old["attempt_id"] != candidate.attempt_id
    {
        return Err(refused(
            "resume restart recovery requires an exact prior runtime attempt",
        ));
    }
    let mut db = existing(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    owner_is_current(&tx, runtime)?;
    let retained: Option<String> = tx
        .query_row(RECORD, [&candidate.attempt_id], |r| r.get(0))
        .optional()?;
    if retained.as_deref() != Some(&candidate.original) {
        return Err(refused("resume restart attempt changed during observation"));
    }
    crate::ingress::stop::control::require_unheld_in(&tx, &candidate.target)?;
    super::response::require_unheld_in(&tx, &candidate.target)?;
    let occupied: bool = tx.query_row(
        "SELECT
        EXISTS(SELECT 1 FROM codex_turn_queue WHERE target_thread_id=?1 AND state<>'pending')
        OR EXISTS(SELECT 1 FROM codex_mutation_attempts WHERE state='prepared'
            AND attempt_id<>?2 AND (scoped=0 OR target_thread_id=?1))",
        params![candidate.target, candidate.attempt_id],
        |r| r.get(0),
    )?;
    if occupied {
        return Err(refused(
            "resume restart target still has unresolved execution",
        ));
    }
    let jobs = tx
        .prepare(
            "SELECT job_id,last_error FROM codex_turn_queue
        WHERE target_thread_id=? AND state='pending' AND turn_id IS NULL
        AND execution_generation IS NULL AND baseline_turn_ids='[]'
        AND NOT EXISTS(SELECT 1 FROM cdr_execution_holds h WHERE h.job_id=codex_turn_queue.job_id)",
        )?
        .query_map([&candidate.target], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, error)| matching_preflight(error, &candidate.attempt_id))
        .collect::<Vec<_>>();
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
    let proof = json!({"resident":resident.0,"generation":resident.1,
        "metadata":metadata,"latest":latest,"preflight_errors":jobs,
        "resolution":"prior runtime idle resume retired; original reply remains unknown"})
    .to_string();
    if tx.execute(
        "INSERT INTO cdr_resume_restart_recoveries VALUES(?,?,?,?,?)",
        params![
            candidate.attempt_id,
            candidate.original,
            proof,
            runtime,
            now
        ],
    )? != 1
        || tx.execute(
            "DELETE FROM codex_mutation_attempts WHERE attempt_id=? AND state='prepared'",
            [&candidate.attempt_id],
        )? != 1
    {
        return Err(refused("resume restart recovery lost its exact attempt"));
    }
    for (job, error) in &jobs {
        if tx.execute(
            "UPDATE codex_turn_queue SET last_error='',updated_at=?
            WHERE job_id=? AND state='pending' AND turn_id IS NULL AND last_error=?",
            params![now, job, error],
        )? != 1
        {
            return Err(refused("resume restart recovery lost a pending preflight"));
        }
    }
    owner_is_current(&tx, runtime)?;
    tx.commit()?;
    Ok(jobs.len())
}

#[cfg(test)]
mod tests;
