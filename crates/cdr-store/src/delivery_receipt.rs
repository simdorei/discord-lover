//! Durable send-intent and Discord message receipt; unknown outcomes never resend.
use crate::{Result, schema::open_initialized};
use rusqlite::{Connection, TransactionBehavior, params};
use std::path::Path;

mod gateway;
pub use gateway::{candidates_for_hash, confirm_legacy_message_reply, confirmed_message};

#[derive(Debug, PartialEq, Eq)]
pub enum ReceiptState {
    New,
    Delivered(String),
    Unknown,
    ContentConflict,
    RejectedBlocked(String),
    Held(String),
}

pub(crate) fn migrate_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS codex_delivery_receipts (
        receipt_key TEXT PRIMARY KEY, content_hash TEXT NOT NULL, message_id TEXT);",
    )?;
    for (name, definition) in [
        ("retryable", "INTEGER NOT NULL DEFAULT 0"),
        ("blocked_reason", "TEXT"),
    ] {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('codex_delivery_receipts') WHERE name=?)",
            [name], |row| row.get(0))?;
        if !exists {
            connection.execute_batch(&format!(
                "ALTER TABLE codex_delivery_receipts ADD COLUMN {name} {definition}"
            ))?;
        }
    }
    Ok(())
}
pub(crate) fn schema_current(connection: &Connection) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT COUNT(*)=2 FROM pragma_table_info('codex_delivery_receipts') WHERE name IN ('retryable','blocked_reason')",
        [],
        |row| row.get(0),
    )?)
}
pub fn begin(path: &Path, key: &str, content_hash: &str) -> Result<ReceiptState> {
    begin_guarded(path, key, content_hash, None)
}

pub fn begin_guarded(
    path: &Path,
    key: &str,
    content_hash: &str,
    guard: Option<&crate::new_reply::DeliveryGuard<'_>>,
) -> Result<ReceiptState> {
    let mut connection = open_initialized(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let state = begin_guarded_in(&transaction, key, content_hash, guard)?;
    if !matches!(state, ReceiptState::Held(_)) {
        transaction.commit()?;
    }
    Ok(state)
}

pub(crate) fn begin_guarded_in(
    transaction: &rusqlite::Transaction<'_>,
    key: &str,
    content_hash: &str,
    guard: Option<&crate::new_reply::DeliveryGuard<'_>>,
) -> Result<ReceiptState> {
    let parsed: serde_json::Value = serde_json::from_str(key).unwrap_or(serde_json::Value::Null);
    let channel = parsed
        .get(0)
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0);
    let invalid_scoped = channel.is_none()
        && transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM cdr_archived_room_dispositions)",
            [],
            |r| r.get::<_, bool>(0),
        )?;
    if invalid_scoped
        || crate::archived_cleanup::blocked_in(transaction, channel, None)?
        || crate::archived_cleanup::record_retired_in(transaction, "codex_delivery_receipts", key)?
    {
        return Ok(ReceiptState::Held(
            "invalid receipt identity or archived mirror retired; no automatic replay".into(),
        ));
    }
    if crate::archived_cleanup::receipt::retired(transaction, key)? {
        return Ok(ReceiptState::Held(
            "original delivery work retired; no automatic replay".into(),
        ));
    }
    crate::final_recovery::validate_claim_in(transaction, key, content_hash, guard)?;
    if let Some(reason) =
        crate::new_reply::validate_claim_in(transaction, key, content_hash, guard)?
    {
        return Ok(ReceiptState::Held(reason));
    }
    let inserted = transaction.execute(
        "INSERT OR IGNORE INTO codex_delivery_receipts (receipt_key,content_hash) VALUES (?,?)",
        params![key, content_hash],
    )? == 1;
    let (hash, message, retryable, blocked): (String, Option<String>, bool, Option<String>) = transaction.query_row(
        "SELECT content_hash,message_id,retryable,blocked_reason FROM codex_delivery_receipts WHERE receipt_key=?",
        [key],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let claimed_retry = hash == content_hash
        && message.is_none()
        && blocked.is_none()
        && retryable
        && transaction.execute(
            "UPDATE codex_delivery_receipts SET retryable=0 WHERE receipt_key=? AND retryable=1",
            [key],
        )? == 1;
    if message.is_some() && hash == content_hash {
        crate::new_reply::confirm_receipt_in(transaction, key)?;
    }
    crate::new_reply::notice_claimed_in(transaction, key)?;
    Ok(if hash != content_hash {
        ReceiptState::ContentConflict
    } else if inserted || claimed_retry {
        ReceiptState::New
    } else if let Some(message) = message {
        ReceiptState::Delivered(message)
    } else if let Some(reason) = blocked {
        ReceiptState::RejectedBlocked(reason)
    } else {
        ReceiptState::Unknown
    })
}
pub fn confirm(path: &Path, key: &str, message_id: &str) -> Result<bool> {
    let mut connection = open_initialized(path)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let changed = tx.execute("UPDATE codex_delivery_receipts SET message_id=? WHERE receipt_key=? AND (message_id IS NULL OR message_id=?)",params![message_id,key,message_id])?==1;
    if changed {
        crate::new_reply::confirm_receipt_in(&tx, key)?;
    }
    tx.commit()?;
    Ok(changed)
}
pub fn unknown_count(path: &Path) -> Result<i64> {
    Ok(open_initialized(path)?.query_row(
        "SELECT COUNT(*) FROM cdr_live_codex_delivery_receipts WHERE message_id IS NULL AND retryable=0 AND blocked_reason IS NULL",
        [],
        |row| row.get(0),
    )?)
}
/// Call only after an authoritative rejection that could not create a message.
pub fn release_rejected(path: &Path, key: &str) -> Result<bool> {
    let mut connection = open_initialized(path)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE codex_delivery_receipts SET retryable=1 WHERE receipt_key=? AND message_id IS NULL AND blocked_reason IS NULL",
        [key],
    )? == 1;
    if changed {
        crate::new_reply::notice_rejected_in(&tx, key)?;
    }
    tx.commit()?;
    Ok(changed)
}

/// A definite rejection requiring operator correction must not spin on every tick.
pub fn block_rejected(path: &Path, key: &str, reason: &str) -> Result<bool> {
    let reason: String = reason.chars().take(1000).collect();
    Ok(open_initialized(path)?.execute(
        "UPDATE codex_delivery_receipts SET blocked_reason=?,retryable=0 WHERE receipt_key=? AND message_id IS NULL",
        params![reason,key])? == 1)
}

pub fn blocked_count(path: &Path) -> Result<i64> {
    Ok(open_initialized(path)?.query_row(
        "SELECT COUNT(*) FROM cdr_live_codex_delivery_receipts WHERE message_id IS NULL AND blocked_reason IS NOT NULL",
        [], |row| row.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_confirmation_accepts_same_message_but_never_replaces_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("receipt.sqlite");
        assert_eq!(begin(&path, "chunk", "hash").unwrap(), ReceiptState::New);
        assert!(confirm(&path, "chunk", "123").unwrap());
        assert!(confirm(&path, "chunk", "123").unwrap());
        assert!(!confirm(&path, "chunk", "456").unwrap());
        assert_eq!(
            begin(&path, "chunk", "hash").unwrap(),
            ReceiptState::Delivered("123".into())
        );
    }
    #[test]
    fn definite_blocked_rejection_survives_reopen_without_new_attempt_or_unknown_count() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("store.sqlite");
        assert_eq!(begin(&path, "chunk", "hash").unwrap(), ReceiptState::New);
        assert!(block_rejected(&path, "chunk", "Discord permission denied").unwrap());
        assert_eq!(
            begin(&path, "chunk", "hash").unwrap(),
            ReceiptState::RejectedBlocked("Discord permission denied".into())
        );
        assert!(!release_rejected(&path, "chunk").unwrap());
        assert_eq!(unknown_count(&path).unwrap(), 0);
        assert_eq!(blocked_count(&path).unwrap(), 1);
    }
    #[test]
    fn confirmed_rejection_does_not_allow_payload_to_change_on_retry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("store.sqlite");
        assert_eq!(
            begin(&path, "chunk", "original").unwrap(),
            ReceiptState::New
        );
        assert!(release_rejected(&path, "chunk").unwrap());
        assert_eq!(
            begin(&path, "chunk", "changed").unwrap(),
            ReceiptState::ContentConflict
        );
        assert_eq!(
            begin(&path, "chunk", "original").unwrap(),
            ReceiptState::New
        );
        assert_eq!(
            begin(&path, "chunk", "original").unwrap(),
            ReceiptState::Unknown
        );
    }
    #[test]
    fn reopen_never_resends_an_unknown_send_and_retains_confirmed_message_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("store.sqlite");
        assert_eq!(begin(&path, "chunk", "hash").unwrap(), ReceiptState::New);
        assert_eq!(
            begin(&path, "chunk", "hash").unwrap(),
            ReceiptState::Unknown
        );
        assert_eq!(unknown_count(&path).unwrap(), 1);
        assert!(confirm(&path, "chunk", "123").unwrap());
        assert_eq!(
            begin(&path, "chunk", "hash").unwrap(),
            ReceiptState::Delivered("123".into())
        );
        assert_eq!(
            begin(&path, "chunk", "different").unwrap(),
            ReceiptState::ContentConflict
        );
        assert_eq!(unknown_count(&path).unwrap(), 0);
    }
}
