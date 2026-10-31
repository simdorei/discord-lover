//! Durable archive scope, serialized with ingress and prompt ownership writes.
//! No lease expiry or automatic replay: an interrupted dispatch stays fenced.
mod schema;
pub(crate) use schema::{migrate_schema, schema_current};

use crate::{Result, StoreError};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Debug, PartialEq, Eq)]
pub struct Reservation {
    pub operation_id: String,
    members: BTreeMap<String, String>,
}

pub fn reserve(path: &Path, scope: &BTreeSet<String>, own: Option<&str>) -> Result<Reservation> {
    if scope.is_empty() || scope.iter().any(|id| id.is_empty() || id.trim() != id) {
        return Err(StoreError::Integrity(
            "invalid archive reservation scope".into(),
        ));
    }
    let mut db = crate::schema::open_initialized(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Archive is an explicit terminal action. Existing ingress, queued input and
    // earlier archive attempts remain evidence, not reasons to reject it.
    let operation = uuid::Uuid::new_v4().to_string();
    let mut members = BTreeMap::new();
    for target in scope {
        tx.execute(
            "INSERT INTO codex_archive_fences (target_thread_id,operation_id,own_ingress_id,phase)
             VALUES (?,?,?,'attempted') ON CONFLICT(target_thread_id) DO NOTHING",
            params![target, operation, own],
        )?;
        let owner = tx.query_row(
            "SELECT operation_id FROM codex_archive_fences WHERE target_thread_id=?",
            [target],
            |row| row.get(0),
        )?;
        members.insert(target.clone(), owner);
    }
    tx.commit()?;
    Ok(Reservation {
        operation_id: operation,
        members,
    })
}

pub fn target_is_fenced(path: &Path, target_thread_id: &str) -> Result<bool> {
    Ok(crate::schema::open_initialized(path)?.query_row(
        "SELECT EXISTS(SELECT 1 FROM codex_archive_fences WHERE target_thread_id=?1)",
        [target_thread_id],
        |row| row.get(0),
    )?)
}

/// Only after every scope member's persisted archived state was verified.
/// The target fence and all held requests remain; no implicit unarchive/replay.
pub fn verified(path: &Path, reservation: &Reservation) -> Result<()> {
    let mut db = crate::schema::open_initialized(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (target, operation) in &reservation.members {
        if tx.execute("UPDATE codex_archive_fences SET phase='verified' WHERE target_thread_id=? AND operation_id=?", params![target, operation])? != 1 {
            return Err(StoreError::Integrity("archive reservation changed before verification".into()));
        }
    }
    tx.commit()?;
    Ok(())
}

/// Call only for a proven pre-effect rejection, never a timeout or disconnect.
/// Release only new fences. A rejected manual retry cannot clear a prior unknown
/// archive outcome or the separately held ingress.
pub fn release_rejected(path: &Path, reservation: &Reservation) -> Result<()> {
    let mut db = crate::schema::open_initialized(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let expected = reservation
        .members
        .values()
        .filter(|id| **id == reservation.operation_id)
        .count();
    if tx.execute(
        "DELETE FROM codex_archive_fences WHERE operation_id=? AND phase='attempted'",
        [&reservation.operation_id],
    )? != expected
    {
        return Err(StoreError::Integrity(
            "missing rejected archive reservation".into(),
        ));
    }
    tx.commit()?;
    Ok(())
}

/// Validate archive custody at the actual writer without consuming or resolving
/// any prior request. The durable archive reservation owns uncertain outcomes.
pub fn validate_dispatch(
    path: &Path,
    runtime: &str,
    target: &str,
    origin: Option<&serde_json::Value>,
) -> Result<()> {
    let mut db = crate::schema::open_initialized(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: Option<String> = tx
        .query_row(
            "SELECT runtime_id FROM codex_mutation_runtime WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let reserved: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM codex_archive_fences WHERE target_thread_id=?)",
        [target],
        |r| r.get(0),
    )?;
    if current.as_deref() != Some(runtime) || !reserved {
        return Err(StoreError::Integrity(
            "archive has no current durable reservation".into(),
        ));
    }
    crate::ingress::stop::revision::validate_request_in(
        &tx,
        "thread/archive",
        Some(target),
        origin,
    )?;
    tx.commit()?;
    Ok(())
}
