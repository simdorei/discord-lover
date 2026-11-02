use super::{Authority, Scope, authority, digest, refused};
use crate::Result;
use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::Value;
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn begin(path: &Path, scope: &Scope<'_>, value: &Value, payload: &Value) -> Result<()> {
    let claim: Authority = serde_json::from_value(value.clone())?;
    let mut db = super::super::existing(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if crate::archived_cleanup::record_retired_in(&tx, "codex_turn_queue", &claim.job)?
        || crate::archived_cleanup::blocked_in(&tx, None, Some(&claim.thread))?
    {
        return Err(refused());
    }
    authority::validate(&tx, scope, &claim, "")?;
    // Only proved terminal history may be pruned; never expire uncertain sends.
    tx.execute(
        "DELETE FROM cdr_server_responses WHERE request_key IN (
        SELECT request_key FROM cdr_server_responses WHERE phase='terminal'
        ORDER BY updated_at DESC,request_key DESC LIMIT -1 OFFSET 256)",
        [],
    )?;
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM cdr_live_cdr_server_responses",
        [],
        |row| row.get(0),
    )?;
    if count >= 1024 {
        return Err(refused());
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
    let hash = digest(payload)?;
    let original = serde_json::to_string(&claim)?;
    if tx.execute(
        "INSERT INTO cdr_server_responses(request_key,runtime_id,resident_owner,generation,
        target_thread_id,turn_id,job_id,authority_json,response_sha256,phase,created_at,updated_at)
        VALUES(?,?,?,?,?,?,?,?,?,'admitted',?,?)",
        params![
            claim.key,
            claim.runtime,
            claim.resident,
            claim.generation,
            claim.thread,
            claim.turn,
            claim.job,
            original,
            hash,
            now,
            now
        ],
    )? != 1
    {
        return Err(refused());
    }
    authority::validate(&tx, scope, &claim, &claim.key)?;
    if !retained(&tx, &claim, &hash, "admitted")? {
        return Err(refused());
    }
    tx.commit()?;
    Ok(())
}

fn retained(db: &Connection, claim: &Authority, hash: &str, phase: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM cdr_server_responses
        WHERE request_key=? AND runtime_id=? AND resident_owner=? AND generation=?
        AND authority_json=? AND response_sha256=? AND phase=?
        AND target_thread_id=json_extract(authority_json,'$.thread')
        AND turn_id=json_extract(authority_json,'$.turn')
        AND job_id=json_extract(authority_json,'$.job'))",
        params![
            claim.key,
            claim.runtime,
            claim.resident,
            claim.generation,
            serde_json::to_string(claim)?,
            hash,
            phase
        ],
        |row| row.get(0),
    )?)
}

pub fn finish(
    path: &Path,
    scope: &Scope<'_>,
    value: &Value,
    payload: &Value,
    outcome: &str,
) -> Result<()> {
    if !matches!(outcome, "flushed" | "not_sent") {
        return Err(refused());
    }
    let claim: Authority = serde_json::from_value(value.clone())?;
    authority::check_identity(scope, &claim)?;
    let mut db = super::super::existing(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    super::super::owner_is_current(&tx, scope.runtime)?;
    let hash = digest(payload)?;
    if !retained(&tx, &claim, &hash, "admitted")? && !retained(&tx, &claim, &hash, "terminal")? {
        return Err(refused());
    }
    if tx.execute(
        "UPDATE cdr_server_responses
        SET phase=CASE WHEN phase='terminal' THEN phase ELSE ? END,updated_at=unixepoch()
        WHERE request_key=? AND phase IN ('admitted','terminal')",
        params![outcome, claim.key],
    )? != 1
    {
        return Err(refused());
    }
    super::super::owner_is_current(&tx, scope.runtime)?;
    if !retained(&tx, &claim, &hash, outcome)? && !retained(&tx, &claim, &hash, "terminal")? {
        return Err(refused());
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn record_terminal_in(
    db: &Connection,
    thread: &str,
    turn: &str,
    generation: i64,
    resident: &str,
    payload: &str,
) -> Result<()> {
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return Ok(());
    };
    if value["threadId"] != thread
        || value["turn"]["id"] != turn
        || !matches!(
            value["turn"]["status"].as_str(),
            Some("completed" | "interrupted" | "failed")
        )
    {
        return Ok(());
    }
    db.execute("UPDATE cdr_server_responses SET phase='terminal',terminal_json=?,updated_at=unixepoch()
        WHERE target_thread_id=? AND turn_id=? AND generation=? AND resident_owner=?
        AND EXISTS(SELECT 1 FROM codex_turn_queue q WHERE q.job_id=cdr_server_responses.job_id
            AND q.target_thread_id=cdr_server_responses.target_thread_id AND q.turn_id=cdr_server_responses.turn_id
            AND q.state='running' AND COALESCE(q.turn_observation_generation,q.app_server_generation)=?)",
        params![payload,thread,turn,generation,resident,generation])?;
    Ok(())
}
