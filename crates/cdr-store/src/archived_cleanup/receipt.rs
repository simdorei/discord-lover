//! A restored room never reauthorizes a chunk loaded from retired work.
use crate::Result;
use rusqlite::{Connection, params};

/// A verified HTTP/Gateway result adds evidence; it never reopens old work or
/// rewrites the captured unknown receipt. Caller holds the writer transaction.
pub(crate) fn confirm_retired(
    db: &Connection,
    key: &str,
    message_id: &str,
) -> Result<Option<bool>> {
    if !super::record_retired_in(db, "codex_delivery_receipts", key)? {
        return Ok(None);
    }
    if message_id.is_empty() {
        return Ok(Some(false));
    }
    db.execute(
        "INSERT OR IGNORE INTO cdr_archived_room_receipt_confirmations
         (receipt_key,operation_id,content_hash,message_id,recorded_at)
         SELECT r.receipt_key,e.operation_id,r.content_hash,?2,unixepoch('subsec')
         FROM codex_delivery_receipts r JOIN cdr_archived_room_records e
         ON e.table_name='codex_delivery_receipts' AND e.row_key=json_array(r.receipt_key)
         WHERE r.receipt_key=?1 AND (r.message_id IS NULL OR r.message_id=?2)
         ORDER BY e.rowid LIMIT 1",
        params![key, message_id],
    )?;
    Ok(Some(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM cdr_archived_room_receipt_confirmations c
         JOIN codex_delivery_receipts r ON r.receipt_key=c.receipt_key AND r.content_hash=c.content_hash
         WHERE c.receipt_key=?1 AND c.message_id=?2 AND (r.message_id IS NULL OR r.message_id=?2))",
        params![key, message_id],
        |row| row.get(0),
    )?))
}

pub(crate) fn retired(db: &Connection, key: &str) -> Result<bool> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(key) else {
        return Ok(false);
    };
    let (Some(domain), Some(logical)) = (value[1].as_str(), value[2].as_str()) else {
        return Ok(false);
    };
    let table = match domain {
        "completion/v1" => Some("codex_delivery_outbox"),
        "reserve/start-failure/v1" => Some("codex_turn_queue"),
        "async-question-v1" | "async-question-body-v1" | "async-question-confirmation-v1" => {
            Some("cdr_async_questions")
        }
        "new/verification-notice/v1" => Some("codex_new_first_replies"),
        _ => None,
    };
    if let Some(table) = table {
        return super::record_retired_in(db, table, logical);
    }
    if matches!(domain, "message/reply/v1" | "message/error/v1")
        && let Some(id) = logical
            .strip_prefix("inbound-message/")
            .and_then(|s| s.split_once('/'))
            .map(|v| v.0)
    {
        return super::record_retired_in(db, "discord_ingress_journal", &format!("message:{id}"));
    }
    if matches!(
        domain,
        "completion/goal-progress/v1" | "completion/commentary/v1"
    ) && let Some((thread, rest)) = part(logical)
        && let Some((turn, _)) = part(rest)
    {
        let (table, t, r) = if domain == "completion/goal-progress/v1" {
            ("codex_goal_progress", "$.thread", "$.turn")
        } else {
            ("codex_commentary_outbox", "$.target_thread_id", "$.turn_id")
        };
        return Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM cdr_archived_room_records WHERE table_name=?1 AND json_extract(snapshot_json,?2)=?3 AND json_extract(snapshot_json,?4)=?5)",params![table,t,thread,r,turn],|r|r.get(0))?);
    }
    if domain == "async-question-item-text-v1"
        && let Ok(parts) = serde_json::from_str::<Vec<String>>(logical)
        && parts.len() == 3
    {
        return Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM cdr_archived_room_records WHERE table_name='cdr_async_questions' AND json_extract(snapshot_json,'$.thread_id')=?1 AND json_extract(snapshot_json,'$.turn_id')=?2 AND json_extract(snapshot_json,'$.item_id')=?3)",params![parts[0],parts[1],parts[2]],|r|r.get(0))?);
    }
    Ok(false)
}

fn part(value: &str) -> Option<(&str, &str)> {
    let (length, rest) = value.split_once(':')?;
    let length = length.parse::<usize>().ok()?;
    Some((rest.get(..length)?, rest.get(length..)?.strip_prefix(';')?))
}
