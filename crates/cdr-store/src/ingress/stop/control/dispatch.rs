use super::{StopClaim, StopControl, existing, refused, retained_in};
use crate::Result;
use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::Value;
use std::path::Path;

pub fn claim(
    path: &Path,
    control: &StopControl,
    check_selected: impl Fn() -> Result<()>,
) -> Result<Option<StopClaim>> {
    let mut db = existing(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_original(&tx, control)?;
    check_selected()?;
    let claim = StopClaim {
        control: control.clone(),
        token: uuid::Uuid::new_v4().to_string(),
    };
    let changed = tx.execute(
        "UPDATE cdr_stop_controls SET phase='dispatching',claim_token=?
        WHERE operation_id=? AND phase='accepted' AND NOT EXISTS(
            SELECT 1 FROM cdr_stop_controls WHERE target_thread_id=? AND resident_owner=?
            AND generation=? AND turn_id=? AND claim_token IS NOT NULL)",
        params![
            claim.token,
            control.operation_id,
            control.target,
            control.resident,
            control.generation,
            control.turn
        ],
    )?;
    if changed != 1 {
        return Ok(None);
    }
    validate_claim_in(
        &tx,
        &serde_json::to_value(&claim)?,
        (&control.resident, control.generation),
        &serde_json::json!({"threadId":control.target,"turnId":control.turn}),
    )?;
    check_selected()?;
    tx.commit()?;
    Ok(Some(claim))
}

fn validate_original(db: &Connection, control: &StopControl) -> Result<()> {
    if crate::archived_cleanup::record_retired_in(db, "cdr_stop_controls", &control.operation_id)?
        || crate::archived_cleanup::blocked_in(db, Some(control.channel), Some(&control.target))?
    {
        return Err(refused());
    }
    if !retained_in(db, control)? {
        return Err(refused());
    }
    super::super::validate(
        db,
        super::super::StopScope {
            target: &control.target,
            channel: control.channel,
            owner: control.owner,
        },
        &control.binding,
        None,
    )?;
    let blocked: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM codex_archive_fences WHERE target_thread_id=?1)
        OR EXISTS(SELECT 1 FROM codex_observed_completions WHERE thread_id=?1 AND turn_id=?2)",
        params![control.target, control.turn],
        |row| row.get(0),
    )?;
    if blocked {
        return Err(refused());
    }
    let jobs = control
        .jobs
        .iter()
        .map(|text| Ok((text, serde_json::from_str::<Value>(text)?)))
        .collect::<Result<Vec<_>>>()?;
    let running = jobs
        .iter()
        .filter(|(_, job)| job["state"] == "Running")
        .collect::<Vec<_>>();
    if running.len() != 1 {
        return Err(refused());
    }
    let (original_json, original) = running[0];
    let job_id = original["job_id"].as_str().ok_or_else(refused)?;
    let current = crate::queue::select_job(db, job_id)?;
    if serde_json::to_string(&current)? != original_json.as_str()
        || current.completion_evidence_generation() != control.generation
        || current.turn_id.as_deref() != Some(control.turn.as_str())
        || current.channel_id != control.channel
        || current.owner_user_id != Some(control.owner)
    {
        return Err(refused());
    }
    for (_, job) in jobs {
        let id = job["job_id"].as_str().ok_or_else(refused)?;
        if super::super::hold_snapshot(db, id)?.is_none_or(|hold| hold.0 != control.target) {
            return Err(refused());
        }
    }
    Ok(())
}

/// Called both sides of the final writer evidence update, on that same connection.
pub fn validate_claim_in(
    db: &Connection,
    value: &Value,
    owner: (&str, i64),
    params: &Value,
) -> Result<StopClaim> {
    let claim: StopClaim = serde_json::from_value(value.clone())?;
    let control = &claim.control;
    if control.resident != owner.0
        || control.generation != owner.1
        || params["threadId"] != control.target
        || params["turnId"] != control.turn
        || claim.token.is_empty()
    {
        return Err(refused());
    }
    validate_original(db, control)?;
    let valid: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM cdr_stop_controls
        WHERE operation_id=? AND phase='dispatching' AND claim_token=?)",
        params![control.operation_id, claim.token],
        |row| row.get(0),
    )?;
    if !valid {
        return Err(refused());
    }
    Ok(claim)
}

pub fn begin_wire(
    path: &Path,
    value: &Value,
    owner: (&str, i64),
    request: (&str, &str),
    params: &Value,
) -> Result<()> {
    let mut db = existing(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let claim = validate_claim_in(&tx, value, owner, params)?;
    if request.0.is_empty()
        || request.1.is_empty()
        || tx.execute(
            "UPDATE cdr_stop_controls SET wire_attempt=?,wire_id=?
            WHERE operation_id=? AND claim_token=? AND phase='dispatching'
            AND wire_attempt IS NULL AND wire_id IS NULL",
            params![
                request.0,
                request.1,
                claim.control.operation_id,
                claim.token
            ],
        )? != 1
    {
        return Err(refused());
    }
    validate_claim_in(&tx, value, owner, params)?;
    if !wire_matches(&tx, &claim, owner, request)? {
        return Err(refused());
    }
    tx.commit()?;
    Ok(())
}

fn wire_matches(
    db: &Connection,
    claim: &StopClaim,
    owner: (&str, i64),
    request: (&str, &str),
) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM cdr_stop_controls WHERE operation_id=?
        AND resident_owner=? AND generation=? AND claim_token=? AND wire_attempt=? AND wire_id=?)",
        params![
            claim.control.operation_id,
            owner.0,
            owner.1,
            claim.token,
            request.0,
            request.1
        ],
        |row| row.get(0),
    )?)
}

pub fn finish_wire(
    path: &Path,
    value: &Value,
    owner: (&str, i64),
    request: (&str, &str),
    outcome: &str,
) -> Result<()> {
    if !matches!(outcome, "not_sent" | "reply_ok" | "reply_error") {
        return Err(refused());
    }
    let claim: StopClaim = serde_json::from_value(value.clone())?;
    let mut db = existing(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !retained_in(&tx, &claim.control)? || !wire_matches(&tx, &claim, owner, request)? {
        return Err(refused());
    }
    tx.execute("UPDATE cdr_stop_controls SET phase=CASE WHEN phase='settled' THEN phase ELSE ? END,last_error=?
        WHERE operation_id=? AND claim_token=?",
        params![if outcome=="reply_ok" {"acknowledged"} else {"unknown"},outcome,claim.control.operation_id,claim.token])?;
    if !wire_matches(&tx, &claim, owner, request)? {
        return Err(refused());
    }
    tx.commit()?;
    Ok(())
}

pub fn record_error(path: &Path, claim: &StopClaim, error: &str) -> Result<()> {
    let bounded: String = error.chars().take(1000).collect();
    existing(path)?.execute(
        "UPDATE cdr_stop_controls SET phase='unknown',last_error=?
        WHERE operation_id=? AND claim_token=? AND phase='dispatching'",
        params![bounded, claim.control.operation_id, claim.token],
    )?;
    Ok(())
}
