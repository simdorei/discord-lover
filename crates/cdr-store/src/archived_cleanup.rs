//! Archived mirror routing dispositions are separate from execution outcomes.
//! Original rows, unknown receipts and authorizations are never settled here.
pub mod effects;
mod legacy;
pub(crate) mod receipt;
mod records;
use crate::{Result, StoreError};
pub(crate) use records::install as install_records;
pub(crate) use records::schema_current as records_current;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

const SCHEMA: &str = include_str!("archived_cleanup/schema.sql");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub operation_id: String,
    pub channel: i64,
    pub target: String,
    pub parent: i64,
    pub guild: i64,
    pub phase: String,
    pub mapping_json: String,
    pub source_json: String,
}

pub struct Prepare<'a> {
    pub channel: i64,
    pub target: &'a str,
    pub parent: i64,
    pub guild: i64,
    pub origin: &'a serde_json::Value,
    pub now: f64,
}

pub(crate) fn migrate_schema(db: &Connection) -> Result<()> {
    db.execute_batch(SCHEMA)?;
    if !schema_current(db)? {
        return Err(StoreError::Integrity(
            "archived cleanup schema is malformed".into(),
        ));
    }
    Ok(())
}

pub(crate) fn schema_current(db: &Connection) -> Result<bool> {
    // Compare definitions, not just names: a malformed present table or weakened
    // trigger must not gain authority through a cached schema check.
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(SCHEMA)?;
    let definitions = expected
        .prepare("SELECT type,name,sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY name")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (kind, name, sql) in definitions {
        let actual: Option<String> = db
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type=? AND name=?",
                params![kind, name],
                |r| r.get(0),
            )
            .optional()?;
        if actual.as_deref() != Some(sql.as_str()) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn get(path: &Path, channel: i64) -> Result<Option<Intent>> {
    get_in(&crate::schema::open_initialized(path)?, channel)
}

fn get_in(db: &Connection, channel: i64) -> Result<Option<Intent>> {
    Ok(db.query_row("SELECT operation_id,channel_id,target_thread_id,parent_id,guild_id,phase,mapping_json,source_json FROM cdr_archived_room_dispositions WHERE channel_id=? ORDER BY rowid DESC LIMIT 1", [channel], |r| Ok(Intent {
        operation_id:r.get(0)?, channel:r.get(1)?, target:r.get(2)?, parent:r.get(3)?, guild:r.get(4)?, phase:r.get(5)?, mapping_json:r.get(6)?, source_json:r.get(7)?,
    })).optional()?)
}

pub fn blocked(path: &Path, channel: Option<i64>, target: Option<&str>) -> Result<bool> {
    blocked_in(&crate::schema::open_initialized(path)?, channel, target)
}

pub fn unresolved_target(path: &Path, target: &str) -> Result<bool> {
    Ok(crate::schema::open_initialized(path)?.query_row("SELECT EXISTS(SELECT 1 FROM cdr_archived_room_dispositions WHERE target_thread_id=? AND phase NOT IN ('mapping_retired','aborted_source_changed'))",[target],|r|r.get(0))?)
}

pub fn pending(path: &Path) -> Result<Vec<Intent>> {
    let db = crate::schema::open_initialized(path)?;
    let channels = db.prepare("SELECT channel_id FROM cdr_archived_room_dispositions WHERE phase NOT IN ('mapping_retired','aborted_source_changed') ORDER BY rowid")?
        .query_map([], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    channels
        .into_iter()
        .map(|channel| {
            get_in(&db, channel)?
                .ok_or_else(|| StoreError::Integrity("missing cleanup intent".into()))
        })
        .collect()
}

pub fn blocked_in(db: &Connection, channel: Option<i64>, target: Option<&str>) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM cdr_archived_room_dispositions d WHERE
        (channel_id=?1 AND phase<>'aborted_source_changed') OR
        (target_thread_id=?2 AND phase<>'aborted_source_changed' AND
          (phase<>'mapping_retired' OR NOT EXISTS(SELECT 1 FROM mirror_threads m WHERE m.codex_thread_id=?2 AND m.discord_thread_id<>d.channel_id))))", params![channel,target], |r| r.get(0))?)
}

pub fn record_retired_in(db: &Connection, table: &str, key: &str) -> Result<bool> {
    if table == "cdr_stop_controls" {
        return Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM cdr_archived_room_records WHERE table_name=?1 AND json_extract(snapshot_json,'$.operation_id')=?2)",params![table,key],|r|r.get(0))?);
    }
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM cdr_archived_room_records WHERE table_name=?1 AND row_key=json_array(?2))",params![table,key],|r|r.get(0))?)
}

pub fn prepare(
    path: &Path,
    scope: &Prepare<'_>,
    verify: impl FnOnce() -> Result<String>,
) -> Result<Intent> {
    effects::close(|| {
        let mut db = crate::schema::open_initialized(path)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if scope.channel <= 0
            || scope.parent <= 0
            || scope.guild <= 0
            || scope.target.is_empty()
            || !scope.now.is_finite()
        {
            return Err(StoreError::Integrity(
                "invalid archived cleanup scope".into(),
            ));
        }
        if let Some(existing) = get_in(&tx, scope.channel)? {
            if existing.target != scope.target
                || existing.parent != scope.parent
                || existing.guild != scope.guild
            {
                return Err(StoreError::Integrity(
                    "archived cleanup scope changed".into(),
                ));
            }
            if existing.phase != "aborted_source_changed" {
                return Ok(existing);
            }
        }
        crate::mapping::creation::protect_cleanup_in(&tx, scope.channel, Some(scope.target))?;
        crate::mapping::container_creation::protect_cleanup_in(
            &tx,
            scope.channel,
            Some(scope.target),
        )?;
        let mapping = mapping_snapshot(&tx, scope.channel, scope.target, scope.parent)?;
        records::check_conflicts(&tx, scope.channel, scope.target)?;
        let source = verify()?;
        let _: serde_json::Value = serde_json::from_str(&source)?;
        let token = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO cdr_archived_room_dispositions VALUES (?,?,?,?,?,?,?,?,1,'prepared',?)",
            params![
                token,
                scope.channel,
                scope.target,
                scope.parent,
                scope.guild,
                mapping,
                source,
                serde_json::to_string(scope.origin)?,
                scope.now
            ],
        )?;
        let intent = get_in(&tx, scope.channel)?
            .ok_or_else(|| StoreError::Integrity("missing cleanup intent".into()))?;
        records::capture(&tx, &intent)?;
        history(
            &tx,
            &intent,
            "prepared",
            "original records preserved; no automatic replay",
            scope.now,
        )?;
        tx.commit()?;
        Ok(intent)
    })
}

fn mapping_snapshot(db: &Connection, channel: i64, target: &str, parent: i64) -> Result<String> {
    let rows = db.prepare("SELECT json_array(codex_thread_id,project_key,thread_title,discord_channel_id,discord_thread_id,updated_at) FROM mirror_threads WHERE discord_thread_id=?1 OR discord_channel_id=?1 OR codex_thread_id=?2")?
        .query_map(params![channel,target], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let [row] = rows.as_slice() else {
        return Err(StoreError::Integrity(
            "archived cleanup mapping is absent or ambiguous".into(),
        ));
    };
    let value: serde_json::Value = serde_json::from_str(row)?;
    if value[0].as_str() != Some(target)
        || value[3].as_i64() != Some(parent)
        || value[4].as_i64() != Some(channel)
    {
        return Err(StoreError::Integrity(
            "archived cleanup mapping identity changed".into(),
        ));
    }
    Ok(row.clone())
}

pub fn verify_mapping(path: &Path, intent: &Intent) -> Result<()> {
    let db = crate::schema::open_initialized(path)?;
    if mapping_snapshot(&db, intent.channel, &intent.target, intent.parent)? != intent.mapping_json
    {
        return Err(StoreError::Integrity(
            "archived cleanup mapping revision changed".into(),
        ));
    }
    Ok(())
}

pub fn transition(
    path: &Path,
    intent: &Intent,
    next: &str,
    detail: &str,
    now: f64,
) -> Result<Intent> {
    transition_checked(path, intent, next, detail, now, || Ok(()))
}

pub fn transition_checked(
    path: &Path,
    intent: &Intent,
    next: &str,
    detail: &str,
    now: f64,
    verify: impl FnOnce() -> Result<()>,
) -> Result<Intent> {
    let allowed = matches!(
        (intent.phase.as_str(), next),
        (
            "prepared" | "delete_rejected",
            "delete_attempted" | "aborted_source_changed" | "deleted"
        ) | (
            "delete_attempted",
            "delete_unknown" | "delete_rejected" | "deleted"
        ) | ("delete_unknown", "deleted")
            | ("deleted", "mapping_retired")
    );
    if !allowed {
        return Err(StoreError::Integrity(
            "invalid archived cleanup transition".into(),
        ));
    }
    let mut db = crate::schema::open_initialized(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if tx.execute("UPDATE cdr_archived_room_dispositions SET phase=? WHERE operation_id=? AND channel_id=? AND phase=?",params![next,intent.operation_id,intent.channel,intent.phase])?!=1 {
        return Err(StoreError::Integrity("archived cleanup ownership changed".into()));
    }
    records::capture(&tx, intent)?;
    history(&tx, intent, next, detail, now)?;
    if next == "delete_attempted"
        && mapping_snapshot(&tx, intent.channel, &intent.target, intent.parent)?
            != intent.mapping_json
    {
        return Err(StoreError::Integrity(
            "mapping changed before DELETE boundary".into(),
        ));
    }
    verify()?;
    tx.commit()?;
    let mut result = intent.clone();
    result.phase = next.into();
    Ok(result)
}

fn history(db: &Connection, intent: &Intent, phase: &str, detail: &str, now: f64) -> Result<()> {
    db.execute("INSERT INTO cdr_archived_room_history(operation_id,phase,detail,recorded_at) VALUES(?,?,?,?)",params![intent.operation_id,phase,detail,now])?;
    Ok(())
}

pub fn retire_mapping(path: &Path, intent: &Intent, now: f64) -> Result<()> {
    let mut db = crate::schema::open_initialized(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if intent.phase != "deleted"
        || mapping_snapshot(&tx, intent.channel, &intent.target, intent.parent)?
            != intent.mapping_json
    {
        return Err(StoreError::Integrity(
            "archived cleanup mapping retirement requires confirmed deletion and exact revision"
                .into(),
        ));
    }
    legacy::reconcile_in(&tx, intent)?;
    if tx.execute("UPDATE cdr_archived_room_dispositions SET phase='mapping_retired' WHERE operation_id=? AND phase='deleted'",[&intent.operation_id])?!=1 { return Err(StoreError::Integrity("cleanup CAS failed".into())); }
    tx.execute(
        "DELETE FROM mirror_threads WHERE codex_thread_id=? AND discord_thread_id=?",
        params![intent.target, intent.channel],
    )?;
    history(
        &tx,
        intent,
        "mapping_retired",
        "Discord deletion confirmed; original records retained",
        now,
    )?;
    tx.commit()?;
    Ok(())
}

pub fn counts(path: &Path, intent: &Intent) -> Result<BTreeMap<String, i64>> {
    let db = crate::schema::open_initialized(path)?;
    Ok(db.prepare("SELECT table_name,count(*) FROM cdr_archived_room_records WHERE operation_id=? GROUP BY table_name")?
        .query_map([&intent.operation_id],|r| Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
}

pub fn inspection_parent(path: &Path, parent: i64) -> Result<bool> {
    Ok(crate::schema::open_initialized(path)?.query_row("SELECT EXISTS(SELECT 1 FROM cdr_archived_room_dispositions d JOIN cdr_archived_room_records e ON e.operation_id=d.operation_id WHERE d.parent_id=? AND e.table_name='discord_ingress_journal')",[parent],|r|r.get(0))?)
}

/// Returns the disposition that actually captured this request, not a newer room operation.
pub fn ingress_operation(path: &Path, ingress: &str) -> Result<Option<String>> {
    Ok(crate::schema::open_initialized(path)?.query_row("SELECT operation_id FROM cdr_archived_room_records WHERE table_name='discord_ingress_journal' AND row_key=json_array(?) ORDER BY rowid LIMIT 1", [ingress], |r|r.get(0)).optional()?)
}
