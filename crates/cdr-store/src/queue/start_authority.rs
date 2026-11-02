//! Original queue authority, checked inside the final mutation writer transaction.
use rusqlite::{Connection, params};
use serde_json::Value;

use super::{QueueJobState, read::select_job};
use crate::{Result, StoreError};

/// Use only the connection supplied by `mutation_attempt::begin_checked`. This
/// does not refresh a claim, change a row, open a connection, or grant a retry.
pub fn validate_in(
    connection: &Connection,
    claim: &Value,
    target: &str,
    generation: i64,
) -> Result<()> {
    let job_id = claim
        .get("job_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(revoked)?;
    let current = select_job(connection, job_id)?;
    if crate::archived_cleanup::record_retired_in(connection, "codex_turn_queue", job_id)?
        || crate::archived_cleanup::blocked_in(connection, Some(current.channel_id), Some(target))?
    {
        return Err(revoked());
    }
    let actual = serde_json::to_value(&current)?;
    // Delivery-only queued/ack_sent flags are not execution authority.
    let fields = [
        "job_id",
        "target_thread_id",
        "channel_id",
        "owner_user_id",
        "discord_message_id",
        "app_server_generation",
        "execution_generation",
        "turn_observation_generation",
        "goal_waiting",
        "prompt",
        "state",
        "attempt_count",
        "turn_id",
        "baseline_turn_ids",
        "last_error",
        "created_at",
        "updated_at",
    ];
    if current.target_thread_id != target
        || current.app_server_generation != generation
        || current.execution_generation != Some(generation)
        || current.state != QueueJobState::Starting
        || current.turn_id.is_some()
        || current.goal_waiting
        || fields
            .iter()
            .any(|field| actual.get(*field) != claim.get(*field))
        || !crate::dead_generation::job_can_mutate(connection, &current)?
    {
        return Err(revoked());
    }
    crate::execution_hold::require_unheld_in(connection, job_id)?;
    let fenced: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM codex_request_cancellations
         WHERE job_id=?1 OR (?2 IS NOT NULL AND discord_message_id=?2))
         OR EXISTS(SELECT 1 FROM codex_archive_fences WHERE target_thread_id=?3)",
        params![job_id, current.discord_message_id, target],
        |row| row.get(0),
    )?;
    if fenced {
        return Err(revoked());
    }
    super::fork_handoff::ensure_no_unresolved_handoff(connection, target)?;
    super::fork_handoff::ensure_source_not_moved(connection, target)?;
    crate::async_resolution::assert_admission_in(connection, target)?;
    Ok(())
}

fn revoked() -> StoreError {
    StoreError::Integrity("original queue start authority changed or is held; no replay".into())
}
