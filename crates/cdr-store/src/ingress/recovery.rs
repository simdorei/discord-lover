use std::path::Path;

use rusqlite::{Connection, TransactionBehavior, params};

use super::read::{get_in, unfinished_prior};
use crate::{Result, StoreError};

pub fn hold(path: &Path, key: &str, reason: &str, not_executed: bool, now: f64) -> Result<()> {
    let mut connection = crate::schema::open_initialized(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    hold_in(&transaction, key, reason, not_executed, now)?;
    transaction.commit()?;
    Ok(())
}

/// Call only under the exclusive runtime guard, before accepting new work.
/// This function never submits anything to Codex or changes old queue rows.
pub fn recover_prior_runtime(path: &Path, runtime_id: &str, now: f64) -> Result<usize> {
    let mut connection = crate::schema::open_initialized(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let records = unfinished_prior(&transaction, runtime_id)?;
    transaction.execute(
        "UPDATE codex_new_first_replies SET ack_recovery_allowed=1
        WHERE confirmation_delivered=0 AND ingress_id IN
        (SELECT ingress_id FROM discord_ingress_journal WHERE runtime_id IS NOT ?)",
        [runtime_id],
    )?;
    for record in &records {
        hold_in(
            &transaction,
            &record.ingress_id,
            "runtime ended before custody handoff or confirmation was recorded",
            matches!(record.state.as_str(), "staged" | "acknowledged"),
            now,
        )?;
    }
    transaction.commit()?;
    Ok(records.len())
}

pub(crate) fn retire_unowned_in(connection: &Connection) -> Result<()> {
    // Retirement runs before any new runtime intake; missing ownership never proves headless work.
    let now: f64 = connection.query_row("SELECT CAST(unixepoch() AS REAL)", [], |r| r.get(0))?;
    for record in unfinished_prior(connection, "manual-reserve-retirement-v1")? {
        if record.owner_id.is_none() {
            hold_in(
                connection,
                &record.ingress_id,
                "legacy unowned request retained during manual Reserve retirement",
                matches!(record.state.as_str(), "staged" | "acknowledged"),
                now,
            )?;
        }
    }
    Ok(())
}

fn hold_in(
    connection: &Connection,
    key: &str,
    reason: &str,
    not_executed: bool,
    now: f64,
) -> Result<()> {
    let record = get_in(connection, key)?
        .ok_or_else(|| StoreError::Integrity(format!("missing ingress hold: {key}")))?;
    // A closed archived mirror retains exact non-dispatch evidence, not a
    // fictional confirmation. Do not stage a new POST into that closed room.
    if crate::room_cleanup::archived_rejections::preserves(connection, key)? {
        return Ok(());
    }
    // A permanent ownership receipt survives removal of its transient queue row.
    // Its established queue/intake machinery owns recovery, not this journal.
    if record.owner_id.is_some() {
        connection.execute(
            "UPDATE codex_new_first_replies SET ack_recovery_allowed=1
            WHERE ingress_id=? AND confirmation_delivered=0",
            [key],
        )?;
        return Ok(());
    }
    if let Some(detail) = known_refusal_detail(&record, reason) {
        // The action's refusal is known; only its notification can be uncertain.
        // Never send a different-key Saved POST or fabricate delivery evidence.
        connection.execute(
            "UPDATE discord_ingress_journal SET state='held',hold_reason=?,updated_at=? WHERE ingress_id=?",
            params![detail, now, key],
        )?;
        return Ok(());
    }
    let description = if record.state == "completed" || record.phase == "result_recorded" {
        "The action completed, but its confirmation was not recorded. It will not execute again."
    } else if not_executed || record.phase == "archive_fenced" {
        "The request was saved but was not executed. It will not be retried automatically."
    } else {
        "The execution outcome is unknown. The request was saved and will not be retried automatically."
    };
    // Callers supply a public-safe phase reason, not raw HTTP/backend error data.
    let reason: String = if record.phase == "archive_fenced" {
        record.hold_reason.as_str()
    } else {
        reason
    }
    .chars()
    .take(500)
    .collect();
    connection.execute(
        "UPDATE discord_ingress_journal SET state='held',hold_reason=?,updated_at=? WHERE ingress_id=?",
        params![reason,now,key],
    )?;
    let already_staged: bool = connection.query_row(
        "SELECT notice_staged FROM discord_ingress_journal WHERE ingress_id=?",
        [key],
        |row| row.get(0),
    )?;
    if already_staged {
        return Ok(());
    }
    let notice_id = format!("ingress-hold:{key}");
    let attachment_note = record
        .payload
        .get("attachments")
        .and_then(serde_json::Value::as_array)
        .filter(|attachments| !attachments.is_empty())
        .map_or(
            "",
            |_| "\nAttachment metadata is saved; unavailable files need re-upload.",
        );
    let content = format!(
        "Saved Discord request requires review\nrequest_id: {key}\n{description}\nphase: {}\ntarget: {}\nreason: {reason}\nUse !runners to inspect your saved requests.{attachment_note}",
        record.phase,
        record
            .target_thread_id
            .as_deref()
            .unwrap_or("not yet known"),
    );
    connection.execute(
        "INSERT INTO codex_delivery_outbox
         (delivery_id,job_id,target_thread_id,turn_id,channel_id,content,created_at,updated_at)
         VALUES (?,?,?,?,?,?,?,?)",
        params![
            notice_id,
            notice_id,
            record.target_thread_id.as_deref().unwrap_or(""),
            notice_id,
            record.channel_id,
            content,
            now,
            now
        ],
    )?;
    connection.execute(
        "UPDATE discord_ingress_journal SET notice_staged=1 WHERE ingress_id=?",
        [key],
    )?;
    Ok(())
}

fn known_refusal_detail(record: &super::StoredIngress, reason: &str) -> Option<String> {
    if let Some(refusal) = super::CleanupRefusal::from_outcome(record.outcome.as_ref()) {
        return Some(if record.hold_reason.starts_with("Mirror sync stopped:") {
            record.hold_reason.clone()
        } else {
            format!(
                "{} Recovery boundary: {}",
                refusal.pending_summary(),
                reason.chars().take(150).collect::<String>()
            )
        });
    }
    super::AsyncAnswerRefusal::from_outcome(record.outcome.as_ref())
        .map(|refusal| format!("{refusal} Notification remains unconfirmed: {reason}"))
}
