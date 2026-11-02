use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{IngressKind, StoredIngress};
use crate::{Result, StoreError};

const COLUMNS: &str = "ingress_id,kind,event_id,application_id,channel_id,owner_user_id,
    source_message_id,payload_json,runtime_id,state,phase,target_thread_id,canonical_owner,
    owner_kind,owner_id,outcome_json,confirmation_delivered,hold_reason,created_at,updated_at";

pub fn get(path: &Path, key: &str) -> Result<Option<StoredIngress>> {
    get_in(&crate::schema::open_initialized(path)?, key)
}

/// Inspection must neither initialize storage nor decode another actor's payload.
pub fn get_for_owner_readonly(
    path: &Path,
    key: &str,
    channel_id: i64,
    owner_user_id: i64,
) -> Result<Option<StoredIngress>> {
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_millis(250))?;
    let found = crate::schema::schema_version(&connection)?;
    let supported = crate::schema::LATEST_STORE_SCHEMA_VERSION;
    if found != supported {
        return Err(StoreError::ReadOnlySchemaVersion {
            found,
            expected: supported,
        });
    }
    Ok(connection
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM discord_ingress_journal
                      WHERE ingress_id = ?1 AND owner_user_id = ?3
                      AND (channel_id = ?2 OR EXISTS(SELECT 1 FROM cdr_archived_room_dispositions d
                          WHERE d.channel_id=discord_ingress_journal.channel_id AND d.parent_id=?2
                          AND d.phase='mapping_retired'))"
            ),
            params![key, channel_id, owner_user_id],
            read_row,
        )
        .optional()?)
}

pub(super) fn get_in(connection: &Connection, key: &str) -> Result<Option<StoredIngress>> {
    Ok(connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM discord_ingress_journal WHERE ingress_id = ?"),
            [key],
            read_row,
        )
        .optional()?)
}

pub fn by_origin(path: &Path, event_id: i64) -> Result<Option<StoredIngress>> {
    by_origin_in(&crate::schema::open_initialized(path)?, event_id)
}

pub(super) fn by_origin_in(
    connection: &Connection,
    event_id: i64,
) -> Result<Option<StoredIngress>> {
    let rows = connection
        .prepare(&format!(
            "SELECT {COLUMNS} FROM discord_ingress_journal WHERE event_id = ? LIMIT 2"
        ))?
        .query_map([event_id], read_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.len() > 1 {
        return Err(StoreError::Integrity(
            "ambiguous ingress origin identity".into(),
        ));
    }
    Ok(rows.into_iter().next())
}

pub fn list_for_owner(
    path: &Path,
    channel_id: i64,
    owner_user_id: i64,
) -> Result<Vec<StoredIngress>> {
    Ok(crate::schema::open_initialized(path)?
        .prepare(&format!(
            "SELECT {COLUMNS} FROM discord_ingress_journal WHERE owner_user_id = ?2
             AND (channel_id = ?1 OR EXISTS(SELECT 1 FROM cdr_archived_room_dispositions d
                WHERE d.channel_id=discord_ingress_journal.channel_id AND d.parent_id=?1 AND d.phase='mapping_retired'))
             AND (state = 'held' OR (state = 'completed' AND confirmation_delivered = 0)
                OR EXISTS(SELECT 1 FROM cdr_archived_room_dispositions d WHERE d.channel_id=discord_ingress_journal.channel_id))
             ORDER BY created_at DESC, ingress_id LIMIT 20"
        ))?
        .query_map(params![channel_id, owner_user_id], read_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Archive is not allowed to strand a request that has not reached its queue.
/// An unbound target is conservatively relevant: selection happens later.
pub fn unfinished_for_archive(
    path: &Path,
    target: &str,
    own_request: Option<&str>,
) -> Result<Option<String>> {
    Ok(crate::schema::open_initialized(path)?
        .query_row(
            "SELECT ingress_id FROM discord_ingress_journal
         WHERE (target_thread_id=?1 OR target_thread_id IS NULL)
         AND state!='completed' AND NOT(state='owned' AND confirmation_delivered=1)
         AND (?2 IS NULL OR ingress_id!=?2)
         ORDER BY created_at,ingress_id LIMIT 1",
            params![target, own_request],
            |row| row.get(0),
        )
        .optional()?)
}

pub(super) fn unfinished_prior(
    connection: &Connection,
    runtime_id: &str,
) -> Result<Vec<StoredIngress>> {
    Ok(connection
        .prepare(&format!(
            "SELECT {COLUMNS} FROM cdr_live_discord_ingress_journal WHERE runtime_id IS NOT ?
             AND owner_id IS NULL AND state IN ('staged','acknowledged','executing','completed')
             AND confirmation_delivered = 0 ORDER BY created_at, ingress_id"
        ))?
        .query_map([runtime_id], read_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn json_at<T: serde::de::DeserializeOwned>(row: &Row<'_>, column: usize) -> rusqlite::Result<T> {
    let raw: String = row.get(column)?;
    serde_json::from_str(&raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn read_row(row: &Row<'_>) -> rusqlite::Result<StoredIngress> {
    let kind: String = row.get(1)?;
    let kind = match kind.as_str() {
        "message" => IngressKind::Message,
        "interaction" => IngressKind::Interaction,
        "action" => IngressKind::Action,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let outcome: Option<String> = row.get(15)?;
    Ok(StoredIngress {
        ingress_id: row.get(0)?,
        kind,
        event_id: row.get(2)?,
        application_id: row.get(3)?,
        channel_id: row.get(4)?,
        owner_user_id: row.get(5)?,
        source_message_id: row.get(6)?,
        payload: json_at(row, 7)?,
        runtime_id: row.get(8)?,
        state: row.get(9)?,
        phase: row.get(10)?,
        target_thread_id: row.get(11)?,
        canonical_owner: row.get(12)?,
        owner_kind: row.get(13)?,
        owner_id: row.get(14)?,
        outcome: outcome.map(|_| json_at(row, 15)).transpose()?,
        confirmation_delivered: row.get(16)?,
        hold_reason: row.get(17)?,
        created_at: row.get(18)?,
        updated_at: row.get(19)?,
    })
}
