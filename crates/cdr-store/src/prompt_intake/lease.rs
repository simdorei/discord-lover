use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{TransactionBehavior, params};
use uuid::Uuid;

use super::{PromptIntakeClaim, StoredPromptIntake, storage};
use crate::queue::UNRESOLVED_FORK_ERROR_PREFIX;
use crate::schema::open_initialized;
use crate::{Result, StoreError};

pub fn try_claim_prompt_intake(
    path: &Path,
    job_id: &str,
    now: f64,
    claim_expires_at: f64,
) -> Result<Option<PromptIntakeClaim>> {
    validate_lease(now, claim_expires_at)?;
    let claim_token = Uuid::new_v4().to_string();
    let mut connection = open_initialized(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    storage::ensure_schema(&transaction)?;
    if crate::archived_cleanup::record_retired_in(&transaction, "codex_prompt_intakes", job_id)? {
        return Ok(None);
    }
    if crate::execution_hold::reason_in(&transaction, job_id)?.is_some() {
        return Ok(None);
    }
    let updated = transaction.execute(
        "UPDATE codex_prompt_intakes SET claim_token = ?, claim_expires_at = ?, updated_at = ? \
         WHERE job_id = ? AND retry_after <= ? AND claim_expires_at <= ? \
         AND NOT EXISTS (SELECT 1 FROM codex_dead_generation_holds hold \
             WHERE hold.target_thread_id = codex_prompt_intakes.target_thread_id)",
        params![claim_token, claim_expires_at, now, job_id, now, now],
    )?;
    let claimed = if updated == 1 {
        let intake = storage::by_job(&transaction, job_id)?
            .ok_or_else(|| StoreError::PromptIntakeNotFound(job_id.to_owned()))?;
        Some(PromptIntakeClaim {
            intake,
            claim_token,
        })
    } else {
        None
    };
    transaction.commit()?;
    Ok(claimed)
}

pub fn renew_prompt_intake_claim_if_current(
    path: &Path,
    claim: &PromptIntakeClaim,
    now: f64,
    new_expires_at: f64,
) -> Result<Option<PromptIntakeClaim>> {
    validate_lease(now, new_expires_at)?;
    let mut connection = open_initialized(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    storage::ensure_schema(&transaction)?;
    let updated = transaction.execute(
        "UPDATE codex_prompt_intakes SET claim_expires_at = ?, updated_at = ? \
         WHERE job_id = ? AND claim_token = ? AND claim_expires_at > ? \
         AND claim_expires_at < ?",
        params![
            new_expires_at,
            now,
            claim.intake.job_id,
            claim.claim_token,
            now,
            new_expires_at,
        ],
    )?;
    let renewed = if updated == 1 {
        let intake = storage::by_job(&transaction, &claim.intake.job_id)?
            .ok_or_else(|| StoreError::PromptIntakeNotFound(claim.intake.job_id.clone()))?;
        Some(PromptIntakeClaim {
            intake,
            claim_token: claim.claim_token.clone(),
        })
    } else {
        None
    };
    transaction.commit()?;
    Ok(renewed)
}

/// Releases process leases after the caller has acquired the singleton runtime guard.
/// This must only be called during startup, never by periodic recovery.
pub fn release_all_prompt_intake_claims(path: &Path) -> Result<usize> {
    let mut connection = open_initialized(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    storage::ensure_schema(&transaction)?;
    let released = transaction.execute(
        "UPDATE codex_prompt_intakes SET claim_token = NULL, claim_expires_at = 0 \
         WHERE claim_token IS NOT NULL OR claim_expires_at != 0",
        [],
    )?;
    transaction.commit()?;
    Ok(released)
}

pub fn record_prompt_intake_failure_if_claimed(
    path: &Path,
    claim: &PromptIntakeClaim,
    error: &str,
    retry_after: f64,
) -> Result<Option<StoredPromptIntake>> {
    if !retry_after.is_finite() {
        return Err(StoreError::InvalidPromptIntakeRetry(retry_after));
    }
    let error = bounded_error(error);
    let observed_at = unix_now()?;
    let mut connection = open_initialized(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    storage::ensure_schema(&transaction)?;
    let updated = transaction.execute(
        "UPDATE codex_prompt_intakes SET attempt_count = \
             CASE WHEN attempt_count < 9223372036854775807 THEN attempt_count + 1 \
                  ELSE attempt_count END, \
         last_error = CASE WHEN instr(last_error, ?) = 1 THEN last_error ELSE ? END, \
         retry_after = ?, claim_token = NULL, claim_expires_at = 0, \
         updated_at = ? WHERE job_id = ? AND claim_token = ?",
        params![
            UNRESOLVED_FORK_ERROR_PREFIX,
            error,
            retry_after,
            observed_at,
            claim.intake.job_id,
            claim.claim_token,
        ],
    )?;
    let intake = (updated == 1)
        .then(|| storage::by_job(&transaction, &claim.intake.job_id))
        .transpose()?
        .flatten();
    transaction.commit()?;
    Ok(intake)
}

fn validate_lease(now: f64, claim_expires_at: f64) -> Result<()> {
    if !now.is_finite() || !claim_expires_at.is_finite() || claim_expires_at <= now {
        return Err(StoreError::InvalidPromptIntakeLease {
            now,
            claim_expires_at,
        });
    }
    Ok(())
}

fn bounded_error(error: &str) -> String {
    let error = error.trim();
    let error = if error.is_empty() {
        "prompt intake processing failed without an error message"
    } else {
        error
    };
    error.chars().take(1_000).collect()
}

fn unix_now() -> Result<f64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(StoreError::from)?
        .as_secs_f64())
}
