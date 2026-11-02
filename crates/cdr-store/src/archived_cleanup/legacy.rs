//! Reconcile a captured pre-v3 DELETE fence only after confirmed remote absence.
use super::Intent;
use crate::{Result, StoreError};
use rusqlite::{Connection, OptionalExtension, params};

pub(super) fn reconcile_in(db: &Connection, intent: &Intent) -> Result<()> {
    let captured: Option<String> = db
        .query_row(
            "SELECT snapshot_json FROM cdr_archived_room_records
         WHERE operation_id=?1 AND table_name='cdr_cleanup_fences' AND row_key=json_array(?2)",
            params![intent.operation_id, intent.channel],
            |r| r.get(0),
        )
        .optional()?;
    let current: Option<(String, Option<String>, String)> = db
        .query_row(
            "SELECT token,target_thread_id,phase FROM cdr_cleanup_fences WHERE channel_id=?",
            [intent.channel],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let invalid =
        || StoreError::Integrity("legacy cleanup identity changed; mapping preserved".into());
    match (captured, current) {
        (None, None) => Ok(()),
        (Some(snapshot), Some((token, target, phase))) => {
            let original: serde_json::Value = serde_json::from_str(&snapshot)?;
            if original["token"].as_str() != Some(token.as_str())
                || original["target_thread_id"].as_str() != Some(intent.target.as_str())
                || target.as_deref() != Some(intent.target.as_str())
                || !matches!(original["phase"].as_str(), Some("deleting" | "deleted"))
                || !matches!(phase.as_str(), "deleting" | "deleted")
                || intent.phase != "deleted"
            {
                return Err(invalid());
            }
            if db.execute(
                "UPDATE cdr_cleanup_fences SET phase='deleted'
                 WHERE channel_id=? AND token=? AND target_thread_id=? AND phase=?",
                params![intent.channel, token, intent.target, phase],
            )? != 1
            {
                return Err(invalid());
            }
            Ok(())
        }
        _ => Err(invalid()),
    }
}
