use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::super::StoredQueueJob;
use super::super::read::select_job;
use crate::schema::open_initialized;
use crate::{Result, StoreError};

fn now() -> Result<f64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64())
}

pub fn begin_attempt(
    path: &Path,
    job_id: &str,
    baseline_turn_ids: &[String],
    generation: i64,
) -> Result<StoredQueueJob> {
    let baseline = serde_json::to_string(baseline_turn_ids)?;
    update_job(
        path,
        job_id,
        generation,
        "UPDATE codex_turn_queue SET state = 'starting', execution_generation = app_server_generation, turn_observation_generation = NULL, goal_waiting = 0, \
         attempt_count = CASE WHEN attempt_count < 9223372036854775807 \
             THEN attempt_count + 1 ELSE attempt_count END, \
         turn_id = NULL, baseline_turn_ids = ?, last_error = '', updated_at = ? \
         WHERE job_id = ? AND app_server_generation = ?",
        Some(&baseline),
        None,
    )
}

pub fn try_begin_attempt(
    path: &Path,
    job_id: &str,
    baseline_turn_ids: &[String],
    generation: i64,
) -> Result<Option<StoredQueueJob>> {
    let baseline = serde_json::to_string(baseline_turn_ids)?;
    let mut connection = open_initialized(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    super::super::fork_handoff::ensure_schema(&transaction)?;
    let candidate = match select_job(&transaction, job_id) {
        Ok(candidate) => candidate,
        Err(StoreError::QueueJobNotFound(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    if crate::archived_cleanup::blocked_in(
        &transaction,
        Some(candidate.channel_id),
        Some(&candidate.target_thread_id),
    )? || crate::archived_cleanup::record_retired_in(&transaction, "codex_turn_queue", job_id)?
    {
        return Ok(None);
    }
    crate::async_resolution::assert_admission_in(&transaction, &candidate.target_thread_id)?;
    if !crate::dead_generation::job_can_mutate(&transaction, &candidate)?
        || crate::execution_hold::reason_in(&transaction, job_id)?.is_some()
        || candidate
            .last_error
            .starts_with(crate::reserve_policy::HOLD_PREFIX)
    {
        return Ok(None);
    }
    let updated = transaction.execute(
        "UPDATE codex_turn_queue SET state = 'starting', execution_generation = ?, turn_observation_generation = NULL, goal_waiting = 0, \
         attempt_count = CASE WHEN attempt_count < 9223372036854775807 \
             THEN attempt_count + 1 ELSE attempt_count END, \
         turn_id = NULL, baseline_turn_ids = ?, last_error = '', updated_at = ? \
         WHERE job_id = ? AND app_server_generation = ? AND state = 'pending' \
         AND NOT EXISTS (SELECT 1 FROM codex_thread_fork_handoffs handoff \
             WHERE handoff.source_thread_id = codex_turn_queue.target_thread_id \
             AND handoff.target_thread_id IS NULL)",
        params![generation, baseline, now()?, job_id, generation],
    )?;
    claimed_result(transaction, job_id, updated)
}

pub fn mark_running(
    path: &Path,
    job_id: &str,
    turn_id: &str,
    generation: i64,
) -> Result<StoredQueueJob> {
    update_job(
        path,
        job_id,
        generation,
        "UPDATE codex_turn_queue SET state = 'running', turn_observation_generation = app_server_generation, goal_waiting = 0, turn_id = ?, updated_at = ? \
         WHERE job_id = ? AND app_server_generation = ?",
        None,
        Some(turn_id),
    )
}

pub fn mark_running_if_claimed(
    path: &Path,
    claimed: &StoredQueueJob,
    turn_id: &str,
) -> Result<Option<StoredQueueJob>> {
    mark_running_with_resident_if_claimed(path, claimed, turn_id, None)
}

/// Resident identity must be captured before dispatch of this exact queue claim.
/// Late stop binding and the observed ACK commit together, or neither commits.
pub fn mark_running_with_resident_if_claimed(
    path: &Path,
    claimed: &StoredQueueJob,
    turn_id: &str,
    resident: Option<&str>,
) -> Result<Option<StoredQueueJob>> {
    let mut connection = open_initialized(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    super::super::fork_handoff::ensure_schema(&transaction)?;
    if !crate::dead_generation::job_can_mutate(&transaction, claimed)? {
        return Ok(None);
    }
    let Some(baseline) = claimed.matching_baseline_json(&transaction)? else {
        return Ok(None);
    };
    let updated = transaction.execute(
        "UPDATE codex_turn_queue SET state = 'running', turn_observation_generation = app_server_generation, goal_waiting = 0, \
         turn_id = ?, updated_at = ? WHERE job_id = ? AND target_thread_id = ? \
         AND app_server_generation = ? AND attempt_count = ? AND updated_at = ? \
         AND baseline_turn_ids = ? AND state = 'starting' AND turn_id IS NULL \
         AND NOT EXISTS (SELECT 1 FROM codex_thread_fork_handoffs handoff \
             WHERE handoff.source_thread_id = codex_turn_queue.target_thread_id \
             AND handoff.target_thread_id IS NULL)",
        params![
            turn_id,
            now()?,
            claimed.job_id,
            claimed.target_thread_id,
            claimed.app_server_generation,
            claimed.attempt_count,
            claimed.updated_at,
            baseline,
        ],
    )?;
    if updated == 1 {
        let running = select_job(&transaction, &claimed.job_id)?;
        if running.turn_id.as_deref() != Some(turn_id) {
            return Err(StoreError::Integrity(
                "queue ACK turn differs from backend response; original Starting preserved".into(),
            ));
        }
        if let Some(resident) = resident {
            crate::ingress::stop::control::bind_late_start_in(
                &transaction,
                claimed,
                &running,
                resident,
            )?;
        }
    }
    claimed_result(transaction, &claimed.job_id, updated)
}

pub fn record_start_failure(
    path: &Path,
    job_id: &str,
    generation: i64,
    error: &str,
    ambiguous: bool,
) -> Result<StoredQueueJob> {
    let mut connection = open_initialized(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let state = if ambiguous { "starting" } else { "pending" };
    let original = select_job(&transaction, job_id)?;
    if !crate::dead_generation::job_can_mutate(&transaction, &original)? {
        return Err(StoreError::DeadGenerationTargetHeld(
            original.target_thread_id,
        ));
    }
    let bounded_error: String = error.trim().chars().take(1_000).collect();
    if transaction.execute(
        "UPDATE codex_turn_queue SET state = ?, last_error = ?, updated_at = ? \
         WHERE job_id = ? AND app_server_generation = ?",
        params![state, bounded_error, now()?, job_id, generation],
    )? != 1
    {
        return Err(StoreError::QueueJobNotFound(job_id.into()));
    }
    let job = select_job(&transaction, job_id)?;
    transaction.commit()?;
    Ok(job)
}

pub fn record_start_failure_if_claimed(
    path: &Path,
    claimed: &StoredQueueJob,
    error: &str,
    ambiguous: bool,
) -> Result<Option<StoredQueueJob>> {
    let bounded_error: String = error.trim().chars().take(1_000).collect();
    let state = if ambiguous { "starting" } else { "pending" };
    let mut connection = open_initialized(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    super::super::fork_handoff::ensure_schema(&transaction)?;
    if !crate::dead_generation::job_can_mutate(&transaction, claimed)? {
        return Ok(None);
    }
    let Some(baseline) = claimed.matching_baseline_json(&transaction)? else {
        return Ok(None);
    };
    let updated = transaction.execute(
        "UPDATE codex_turn_queue SET state = ?, last_error = ?, updated_at = ? \
         WHERE job_id = ? AND target_thread_id = ? AND app_server_generation = ? \
         AND attempt_count = ? AND updated_at = ? AND baseline_turn_ids = ? \
         AND state = 'starting' AND turn_id IS NULL \
         AND NOT EXISTS (SELECT 1 FROM codex_thread_fork_handoffs handoff \
             WHERE handoff.source_thread_id = codex_turn_queue.target_thread_id \
             AND handoff.target_thread_id IS NULL)",
        params![
            state,
            bounded_error,
            now()?,
            claimed.job_id,
            claimed.target_thread_id,
            claimed.app_server_generation,
            claimed.attempt_count,
            claimed.updated_at,
            baseline,
        ],
    )?;
    if updated == 1 && !ambiguous && crate::execution_hold::legacy_or_current_error(error) {
        crate::execution_hold::hold_in(
            &transaction,
            &claimed.job_id,
            &claimed.target_thread_id,
            &bounded_error,
            &serde_json::to_string(claimed)?,
        )?;
        crate::reserve_policy::start_notice::stage_in(&transaction, claimed, &bounded_error)?;
    }
    claimed_result(transaction, &claimed.job_id, updated)
}

impl StoredQueueJob {
    // Call under the IMMEDIATE writer lock; keep original bytes in the final CAS.
    pub(in crate::queue) fn matching_baseline_json(
        &self,
        transaction: &Transaction<'_>,
    ) -> Result<Option<String>> {
        let raw: Option<String> = transaction
            .query_row(
                "SELECT baseline_turn_ids FROM codex_turn_queue WHERE job_id = ?",
                [&self.job_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(raw) = raw else { return Ok(None) };
        let actual: serde_json::Value = serde_json::from_str(&raw)?;
        let expected = serde_json::to_value(&self.baseline_turn_ids)?;
        Ok((actual == expected).then_some(raw))
    }
}

fn claimed_result(
    transaction: rusqlite::Transaction<'_>,
    job_id: &str,
    updated: usize,
) -> Result<Option<StoredQueueJob>> {
    let job = (updated == 1)
        .then(|| select_job(&transaction, job_id))
        .transpose()?;
    if let Some(job) = &job {
        crate::new_reply::bind_running_in(&transaction, job)?;
        crate::mirror::record_job_origin(&transaction, job)?;
    }
    transaction.commit()?;
    Ok(job)
}

fn update_job(
    path: &Path,
    job_id: &str,
    generation: i64,
    sql: &str,
    baseline: Option<&str>,
    turn_id: Option<&str>,
) -> Result<StoredQueueJob> {
    let mut connection = open_initialized(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    crate::execution_hold::require_unheld_in(&transaction, job_id)?;
    let original = select_job(&transaction, job_id)?;
    if !crate::dead_generation::job_can_mutate(&transaction, &original)? {
        return Err(StoreError::DeadGenerationTargetHeld(
            original.target_thread_id,
        ));
    }
    let first = baseline.or(turn_id).unwrap_or_default();
    if transaction.execute(sql, params![first, now()?, job_id, generation])? != 1 {
        return Err(StoreError::QueueJobNotFound(job_id.into()));
    }
    let job = select_job(&transaction, job_id)?;
    crate::new_reply::bind_running_in(&transaction, &job)?;
    crate::mirror::record_job_origin(&transaction, &job)?;
    transaction.commit()?;
    Ok(job)
}
