//! Retire the original stopped requests only after a fresh native terminal read.
use super::{StopControl, existing, refused, retained_in};
use crate::{
    Result,
    queue::{QueueJobState, StoredQueueJob},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

struct HeldJob {
    id: String,
    snapshot: (String, String, String),
}

struct Custody {
    jobs: Vec<StoredQueueJob>,
    holds: Vec<HeldJob>,
}

pub fn recovery_after(path: &Path, after: i64) -> Result<Vec<(i64, StopControl)>> {
    let db = existing(path)?;
    let rows = db
        .prepare(
            "SELECT sequence,record_json FROM cdr_live_cdr_stop_controls c
         WHERE sequence>? AND json_extract(record_json,'$.can_settle')=1 AND
         (phase<>'settled' OR EXISTS(SELECT 1 FROM json_each(record_json,'$.jobs') j
          JOIN codex_turn_queue q ON q.job_id=json_extract(j.value,'$.job_id')))
         ORDER BY sequence LIMIT 16",
        )?
        .query_map([after], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(seq, raw)| Ok((seq, serde_json::from_str(&raw)?)))
        .collect()
}

fn verify_observation(control: &StopControl, observed: &Value) -> Result<()> {
    let goal = observed.get("goal_observation").and_then(|v| v.get("goal"));
    let turns = observed
        .get("turns")
        .and_then(Value::as_array)
        .ok_or_else(refused)?;
    if observed.to_string().len() > 1_048_576
        || observed["threadId"] != control.target
        || observed["truncated"] != false
        || observed["required_owners_complete"] != true
        || observed["thread_observation"]["thread"]["id"] != control.target
        || !matches!(
            observed["thread_observation"]["thread"]["status"]["type"].as_str(),
            Some("idle" | "notLoaded")
        )
        || observed["thread_observation"]["thread"]["archived"] == true
        || !goal.is_some_and(|g| {
            g.is_null() || (g["threadId"] == control.target && g["status"] == "complete")
        })
        || turns.len() != 1
        || turns[0]["id"] != control.turn
        || !matches!(
            turns[0]["status"].as_str(),
            Some("completed" | "interrupted" | "failed")
        )
        || turns[0]["truncated"] == true
    {
        return Err(refused());
    }
    Ok(())
}

/// Caller owns the target/control permit and an IMMEDIATE transaction. Commit
/// only under the same native read lifetime; no network request occurs here.
pub fn recover_terminal_in(
    db: &Connection,
    control: &StopControl,
    observed: &Value,
    reader: &str,
    generation: i64,
    now: f64,
) -> Result<usize> {
    if db.is_autocommit()
        || !control.can_settle
        || control.jobs.is_empty()
        || control.jobs.len() > 128
        || reader.trim().is_empty()
        || reader.len() > 256
        || generation <= 0
        || !now.is_finite()
        || now < 0.0
        || !retained_in(db, control)?
    {
        return Err(refused());
    }
    verify_observation(control, observed)?;
    let scope = super::super::StopScope {
        target: &control.target,
        channel: control.channel,
        owner: control.owner,
    };
    super::super::validate(db, scope, &control.binding, None)?;
    let fenced: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM codex_archive_fences WHERE target_thread_id=?1)
         OR EXISTS(SELECT 1 FROM codex_mutation_attempts WHERE target_thread_id=?1 AND state='prepared')",
        [&control.target], |r| r.get(0),
    )?;
    if fenced {
        return Err(refused());
    }
    let custody = custody_in(db, control)?;
    let mut cancelled = crate::queue::RecoveryCancellation {
        jobs: Vec::new(),
        started_or_uncertain: 0,
    };
    for job in &custody.jobs {
        crate::queue::cancel_request_in(
            db,
            scope,
            now,
            job.job_id.clone(),
            "queue",
            job.discord_message_id,
            &mut cancelled,
        )?;
    }
    // A removed queue row alone is never terminal evidence. Native proof above
    // permits retiring its stop gate while keeping the immutable original hold.
    crate::async_resolution::settle_stopped_execution_in(
        db,
        &control.target,
        &control.turn,
        observed,
        reader,
        generation,
    )?;
    let prior: Option<String> = db.query_row(
        "SELECT terminal_json FROM cdr_stop_controls WHERE operation_id=?",
        [&control.operation_id],
        |r| r.get(0),
    )?;
    let proof =
        json!({"source":"stop_terminal_history_v1","observer":reader,"generation":generation,
        "operation_id":control.operation_id,"observation":observed,"previous_terminal":prior})
        .to_string();
    if db.execute("UPDATE cdr_stop_controls SET phase='settled',terminal_json=? WHERE operation_id=? AND record_json=?",
        params![proof,control.operation_id,serde_json::to_string(control)?])? != 1 {
        return Err(refused());
    }
    for hold in custody.holds {
        if super::super::hold_snapshot(db, &hold.id)?.as_ref() != Some(&hold.snapshot) {
            return Err(refused());
        }
    }
    for job in &custody.jobs {
        let intact: bool = db.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM codex_turn_queue WHERE job_id=?1)
             AND EXISTS(SELECT 1 FROM codex_request_cancellations WHERE job_id=?1
              AND target_thread_id=?2 AND channel_id=?3 AND owner_user_id=?4)",
            params![job.job_id, control.target, control.channel, control.owner],
            |r| r.get(0),
        )?;
        if !intact {
            return Err(refused());
        }
    }
    let saved: Option<String> = db
        .query_row(
            "SELECT terminal_json FROM cdr_stop_controls WHERE operation_id=? AND phase='settled'",
            [&control.operation_id],
            |r| r.get(0),
        )
        .optional()?;
    if saved.as_deref() != Some(&proof) || !retained_in(db, control)? {
        return Err(refused());
    }
    Ok(cancelled.jobs.len())
}

fn custody_in(db: &Connection, control: &StopControl) -> Result<Custody> {
    let mut ids = BTreeSet::new();
    let mut jobs = Vec::new();
    let mut holds = Vec::new();
    for raw in &control.jobs {
        let original: Value = serde_json::from_str(raw)?;
        let id = original["job_id"].as_str().ok_or_else(refused)?;
        if !ids.insert(id.to_owned()) {
            return Err(refused());
        }
        let snapshot = super::super::hold_snapshot(db, id)?.ok_or_else(refused)?;
        if snapshot.0 != control.target {
            return Err(refused());
        }
        holds.push(HeldJob {
            id: id.to_owned(),
            snapshot,
        });
        let present: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM codex_turn_queue WHERE job_id=?)",
            [id],
            |r| r.get(0),
        )?;
        if !present {
            continue;
        }
        let mut current = crate::queue::select_job(db, id)?;
        // Cold recovery may update only the routing generation of an unstarted
        // pending request. Its original prompt and execution identity stay exact.
        if current.state == QueueJobState::Pending
            && current.attempt_count == 0
            && current.execution_generation.is_none()
            && current.turn_id.is_none()
        {
            current.app_server_generation = original["app_server_generation"]
                .as_i64()
                .ok_or_else(refused)?;
        }
        if serde_json::to_string(&current)? != *raw {
            return Err(refused());
        }
        jobs.push(current);
    }
    let active = db
        .prepare(
            "SELECT job_id FROM codex_turn_queue WHERE target_thread_id=? AND state<>'pending'",
        )?
        .query_map([&control.target], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if active.iter().any(|id| !ids.contains(id)) {
        return Err(refused());
    }
    Ok(Custody { jobs, holds })
}
