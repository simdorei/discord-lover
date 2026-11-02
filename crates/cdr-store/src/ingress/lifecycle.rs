use std::path::Path;

use rusqlite::{TransactionBehavior, params};
use serde_json::Value;

use super::read::get_in;
use crate::{Result, StoreError};

pub fn acknowledge(path: &Path, key: &str, now: f64) -> Result<bool> {
    Ok(crate::schema::open_initialized(path)?.execute(
        "UPDATE discord_ingress_journal SET state='acknowledged',phase='acknowledged',updated_at=?
         WHERE ingress_id=? AND state='staged'",
        params![now, key],
    )? == 1)
}

pub fn begin_confirmation(path: &Path, key: &str, now: f64) -> Result<bool> {
    Ok(crate::schema::open_initialized(path)?.execute(
        "UPDATE discord_ingress_journal SET phase='confirmation_retry',updated_at=?
         WHERE ingress_id=? AND state='owned' AND phase='canonical_duplicate' AND owner_kind='prompt'",
        params![now,key],
    )? == 1)
}

pub fn begin_execution(
    path: &Path,
    key: &str,
    phase: &str,
    target: Option<&str>,
    now: f64,
) -> Result<bool> {
    let mut db = crate::schema::open_initialized(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(record) = get_in(&tx, key)?
        && (crate::archived_cleanup::blocked_in(
            &tx,
            Some(record.channel_id),
            record.target_thread_id.as_deref(),
        )? || crate::archived_cleanup::record_retired_in(&tx, "discord_ingress_journal", key)?)
    {
        return Ok(false);
    }
    if let Some(record) = get_in(&tx, key)?
        && let Some(original) = super::frozen_slash_target(&record)
        && (target.is_some_and(|value| value != original)
            || crate::mapping::mirrored_thread_id_in(&tx, Some(record.channel_id))?.as_deref()
                != Some(original))
    {
        return Err(StoreError::Integrity(
            "original slash prompt mapping changed; no execution was claimed".into(),
        ));
    }
    let changed = tx.execute(
        "UPDATE discord_ingress_journal SET state='executing',phase=?,target_thread_id=COALESCE(?,target_thread_id),updated_at=?
         WHERE ingress_id=? AND state IN ('staged','acknowledged')",
        params![phase,target,now,key],
    )? == 1;
    tx.commit()?;
    Ok(changed)
}

pub fn begin_thread_start(path: &Path, key: &str, generation: i64, now: f64) -> Result<bool> {
    if generation <= 0 {
        return Err(StoreError::Integrity(
            "invalid thread/start generation".into(),
        ));
    }
    let mut db = crate::schema::open_initialized(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(record) = get_in(&tx, key)? {
        if crate::archived_cleanup::blocked_in(
            &tx,
            Some(record.channel_id),
            record.target_thread_id.as_deref(),
        )? || crate::archived_cleanup::record_retired_in(&tx, "discord_ingress_journal", key)?
        {
            return Ok(false);
        }
        super::new_execution_prompt(&record)?.ok_or_else(|| {
            StoreError::Integrity("thread/start has no original New input".into())
        })?;
    }
    let changed = tx.execute(
        "UPDATE discord_ingress_journal SET state='executing',phase='thread/start',target_thread_id=NULL,
         outcome_json=json_set(COALESCE(outcome_json,'{}'),'$.thread_start_generation',?),updated_at=? WHERE ingress_id=? AND
         (state IN ('staged','acknowledged') OR (state='executing' AND phase='processing'))",
        params![generation,now,key],
    )? == 1;
    tx.commit()?;
    Ok(changed)
}

pub fn record_created_thread(
    path: &Path,
    key: &str,
    generation: i64,
    target: &str,
    now: f64,
) -> Result<()> {
    if target.trim().is_empty() || target.trim() != target {
        return Err(StoreError::InvalidAppServerManagedTarget(target.into()));
    }
    let changed = crate::schema::open_initialized(path)?.execute(
        "UPDATE discord_ingress_journal SET phase='thread/created',target_thread_id=?,updated_at=?
         WHERE ingress_id=? AND state='executing' AND phase='thread/start'
         AND json_extract(outcome_json,'$.thread_start_generation')=?",
        params![target, now, key, generation],
    )?;
    if changed != 1 {
        return Err(StoreError::Integrity(
            "created thread has no matching generation-bound attempt".into(),
        ));
    }
    Ok(())
}

pub fn record_result(path: &Path, key: &str, outcome: &Value, now: f64) -> Result<()> {
    let mut connection = crate::schema::open_initialized(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let record = get_in(&transaction, key)?
        .ok_or_else(|| StoreError::Integrity(format!("missing ingress result: {key}")))?;
    if record.phase == "cancelled" && transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM codex_request_cancellations WHERE job_id=? AND target_thread_id=? AND owner_user_id=?)",
        params![record.owner_id,record.target_thread_id,record.owner_user_id],
        |row| row.get::<_,bool>(0),
    )? {
        return Err(StoreError::RequestCancelled(key.into()));
    }
    let mut outcome = outcome.clone();
    let stop_receipt = if record.phase == "stop_accepted" {
        Some(
            record
                .outcome
                .as_ref()
                .filter(|value| value.is_object())
                .ok_or_else(|| {
                    StoreError::Integrity("stop acceptance evidence is missing".into())
                })?,
        )
    } else {
        record
            .outcome
            .as_ref()
            .and_then(|value| value.get("stop_receipt"))
    };
    if let Some(saved) = stop_receipt {
        if !outcome.is_object() {
            return Err(StoreError::Integrity(
                "result must preserve stop acceptance evidence".into(),
            ));
        }
        outcome["stop_receipt"] = saved.clone();
    }
    for field in ["new_creation", "new_verification", "new_input"] {
        if let Some(saved) = record.outcome.as_ref().and_then(|value| value.get(field)) {
            if !outcome.is_object() {
                return Err(StoreError::Integrity(
                    "new result must preserve creation evidence".into(),
                ));
            }
            outcome[field] = saved.clone();
        }
    }
    let changed = transaction.execute(
        "UPDATE discord_ingress_journal SET state=CASE WHEN owner_id IS NULL THEN 'completed' ELSE 'owned' END,
         phase='result_recorded',outcome_json=?,
         updated_at=? WHERE ingress_id=? AND state <> 'held'",
        params![outcome.to_string(),now,key],
    )?;
    if changed != 1 {
        return Err(StoreError::Integrity(format!(
            "ingress result cannot be recorded: {key}"
        )));
    }
    transaction.commit()?;
    Ok(())
}

pub fn confirm(path: &Path, key: &str, now: f64) -> Result<()> {
    if let Some(record) = crate::new_reply::get_by_ingress(path, key)?
        && !record.confirmation_delivered
        && record.identity.kind != super::IngressKind::Action
    {
        return Err(StoreError::Integrity(
            "new first reply cannot be confirmed without its normal acknowledgement receipt".into(),
        ));
    }
    let changed = crate::schema::open_initialized(path)?.execute(
        "UPDATE discord_ingress_journal SET confirmation_delivered=1,updated_at=?
         WHERE ingress_id=? AND state IN ('completed','owned')",
        params![now, key],
    )?;
    if changed != 1 {
        return Err(StoreError::Integrity(format!(
            "ingress confirmation has no outcome: {key}"
        )));
    }
    Ok(())
}

/// Freeze routing/processing facts before any asynchronous effect. The original
/// envelope remains immutable; the selected target is recorded separately.
pub fn record_processing_mode(path: &Path, key: &str, mode: &str) -> Result<()> {
    let mut connection = crate::schema::open_initialized(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let record = get_in(&transaction, key)?
        .ok_or_else(|| StoreError::Integrity("missing admitted message custody".into()))?;
    if record.state != "staged" {
        return Err(StoreError::Integrity(
            "message processing mode already frozen".into(),
        ));
    }
    let mut payload = record.payload;
    payload["processing_mode"] = mode.into();
    transaction.execute(
        "UPDATE discord_ingress_journal SET payload_json=? WHERE ingress_id=?",
        params![payload.to_string(), key],
    )?;
    transaction.commit()?;
    Ok(())
}
