use super::{NewReply, get_in};
use crate::{Result, StoreError, schema::open_initialized};
use rusqlite::{TransactionBehavior, params};
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Copy)]
pub struct CheckpointUpdate<'a> {
    pub scan: &'a Value,
    pub verified: bool,
    pub error: &'a str,
    pub now: f64,
}

/// Bounded, durable round-robin discovery does not depend on a surviving queue row.
pub fn pending(path: &Path, limit: usize) -> Result<Vec<NewReply>> {
    let connection = open_initialized(path)?;
    let keys = connection.prepare("SELECT job_id FROM cdr_live_codex_new_first_replies
        WHERE turn_id IS NOT NULL AND (state<>'verified' OR confirmation_delivered=0 OR warning_due=1)
        ORDER BY checked_at,job_id LIMIT ?")?
        .query_map([i64::try_from(limit.min(32)).unwrap_or(32)],|row|row.get::<_,String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    keys.into_iter()
        .filter_map(|key| get_in(&connection, &key).transpose())
        .collect()
}

pub fn checkpoint(path: &Path, original: &NewReply, update: CheckpointUpdate<'_>) -> Result<bool> {
    if !update.now.is_finite() {
        return Err(StoreError::Integrity(
            "invalid verification timestamp".into(),
        ));
    }
    let mut connection = open_initialized(path)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = get_in(&tx, &original.identity.job_id)?
        .ok_or_else(|| StoreError::Integrity("new intent disappeared".into()))?;
    if current.version != original.version
        || current.identity != original.identity
        || current.turn_id != original.turn_id
    {
        return Ok(false);
    }
    let identity_error = super::claim::validate_identity_in(&tx, &current)
        .err()
        .map(|error| error.to_string());
    let overdue = current
        .accepted_at
        .is_some_and(|accepted| update.now >= accepted + 120.0);
    let deadline_error =
        if update.error.is_empty() && overdue && !update.verified && current.state != "verified" {
            "new first input has not been verified after 120 seconds; execution is preserved"
        } else {
            update.error
        };
    let error = identity_error.as_deref().unwrap_or(deadline_error);
    let state = if !error.is_empty() {
        "review_required"
    } else if update.verified || current.state == "verified" {
        "verified"
    } else {
        "pending"
    };
    let warning = overdue && (state != "verified" || !current.confirmation_delivered);
    tx.execute("UPDATE codex_new_first_replies SET state=?,scan_json=?,last_error=?,checked_at=?,version=version+1,
        warning_due=MAX(warning_due,?) WHERE job_id=? AND version=?",
        params![state,update.scan.to_string(),error.chars().take(1000).collect::<String>(),update.now,
            warning,current.identity.job_id,current.version])?;
    tx.commit()?;
    Ok(true)
}

#[must_use]
pub fn warning_text(record: &NewReply) -> String {
    format!(
        "확인 대기\n새 대화의 첫 입력 저장 또는 접수 답장 확인이 지연되고 있습니다. 작업과 답변은 보존하며 다시 실행하지 않습니다.\n새 대화: <#{}>\njob_id: {}",
        record.identity.channel_id, record.identity.job_id
    )
}
