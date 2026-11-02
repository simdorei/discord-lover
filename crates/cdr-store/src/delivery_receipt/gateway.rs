//! Confirmation from independently verified Discord Gateway acknowledgements.
use std::path::Path;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::{Result, schema::open_initialized};

pub fn confirmed_message(path: &Path, key: &str, hash: &str) -> Result<Option<String>> {
    Ok(open_initialized(path)?.query_row(
        "SELECT COALESCE(r.message_id,c.message_id) FROM codex_delivery_receipts r
         LEFT JOIN cdr_archived_room_receipt_confirmations c ON c.receipt_key=r.receipt_key AND c.content_hash=r.content_hash
         WHERE r.receipt_key=? AND r.content_hash=? AND COALESCE(r.message_id,c.message_id) IS NOT NULL",
        params![key, hash], |row| row.get(0),
    ).optional()?)
}

pub fn candidates_for_hash(path: &Path, hash: &str, message_id: &str) -> Result<Vec<String>> {
    let connection = open_initialized(path)?;
    let mut query = connection.prepare(
        "SELECT receipt_key FROM codex_delivery_receipts WHERE content_hash=?
         AND (message_id IS NULL OR message_id=?) AND retryable=0 AND blocked_reason IS NULL",
    )?;
    Ok(query
        .query_map(params![hash, message_id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Only the exact saved normal reply can open a legacy prompt's output barrier.
/// The caller splits `response` with the actual Discord delivery chunker.
pub fn confirm_legacy_message_reply(
    path: &Path,
    channel: u64,
    event: u64,
    response: &str,
    chunks: &[String],
) -> Result<bool> {
    if chunks.is_empty() {
        return Ok(false);
    }
    let (Ok(channel), Ok(event)) = (i64::try_from(channel), i64::try_from(event)) else {
        return Ok(false);
    };
    let mut connection = open_initialized(path)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (index, chunk) in chunks.iter().enumerate() {
        let key = serde_json::to_string(&(
            channel,
            "message/reply/v1",
            format!("inbound-message/{event}/action-result"),
            index,
        ))?;
        let hash = hex::encode(Sha256::digest(chunk.as_bytes()));
        let confirmed: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM codex_delivery_receipts WHERE receipt_key=? AND content_hash=? AND message_id IS NOT NULL)",
            params![key, hash], |row| row.get(0),
        )?;
        if !confirmed {
            return Ok(false);
        }
    }
    let changed = tx.execute(
        "UPDATE discord_ingress_journal SET confirmation_delivered=1,updated_at=unixepoch('subsec')
         WHERE ingress_id=? AND kind='message' AND event_id=? AND channel_id=?
         AND state='owned' AND phase='result_recorded' AND owner_kind='prompt'
         AND confirmation_delivered=0 AND json_extract(outcome_json,'$.response')=?
         AND json_extract(outcome_json,'$.waits_for_final')=1
         AND NOT EXISTS(SELECT 1 FROM codex_new_first_replies WHERE job_id=discord_ingress_journal.owner_id)",
        params![format!("message:{event}"), event, channel, response],
    )? == 1;
    tx.commit()?;
    Ok(changed)
}
