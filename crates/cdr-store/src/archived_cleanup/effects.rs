//! One process-wide admission boundary per canonical database. Lock order:
//! existing target/control permit -> this short mutex -> `SQLite` transaction.
//! No mutex or `SQLite` transaction is held across a network await. A permit only
//! counts already-admitted effects; dropping it never changes durable evidence.
use crate::{Result, StoreError};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

#[derive(Default)]
struct Registry {
    counts: BTreeMap<(PathBuf, String), usize>,
}
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

pub struct Permit {
    keys: Vec<(PathBuf, String)>,
}

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(Mutex::default)
}

pub fn admit(path: &Path, channel: Option<i64>, target: Option<&str>) -> Result<Permit> {
    admit_record(path, channel, target, None)
}

pub fn admit_record(
    path: &Path,
    channel: Option<i64>,
    target: Option<&str>,
    record: Option<(&str, &str)>,
) -> Result<Permit> {
    let mut registry = registry()
        .lock()
        .map_err(|_| StoreError::Integrity("effect admission poisoned".into()))?;
    let db = crate::schema::open_initialized(path)?;
    let path = path.canonicalize()?;
    if super::blocked_in(&db, channel, target)?
        || record
            .map(|(table, key)| super::record_retired_in(&db, table, key))
            .transpose()?
            .unwrap_or(false)
    {
        return Err(StoreError::Integrity(
            "archived mirror retired; original outcome preserved; no automatic replay".into(),
        ));
    }
    let mut keys = Vec::new();
    if let Some(channel) = channel {
        keys.push((path.clone(), format!("room:{channel}")));
    }
    if let Some(target) = target {
        keys.push((path, format!("thread:{target}")));
    }
    for key in &keys {
        *registry.counts.entry(key.clone()).or_default() += 1;
    }
    Ok(Permit { keys })
}

/// Serializes durable closure with final effect admission, not with the whole
/// native turn or a user conversation. Already-admitted effects are drained by
/// the caller using `in_flight` and their bounded transport deadlines.
pub(super) fn close<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let _guard = registry()
        .lock()
        .map_err(|_| StoreError::Integrity("effect admission poisoned".into()))?;
    operation()
}

pub fn in_flight(path: &Path, channel: i64, target: &str) -> Result<usize> {
    let path = path.canonicalize()?;
    let registry = registry()
        .lock()
        .map_err(|_| StoreError::Integrity("effect admission poisoned".into()))?;
    Ok(registry
        .counts
        .get(&(path.clone(), format!("room:{channel}")))
        .copied()
        .unwrap_or(0)
        + registry
            .counts
            .get(&(path, format!("thread:{target}")))
            .copied()
            .unwrap_or(0))
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut registry) = registry().lock() {
            for key in &self.keys {
                if let Some(count) = registry.counts.get_mut(key) {
                    *count -= 1;
                    if *count == 0 {
                        registry.counts.remove(key);
                    }
                }
            }
        }
    }
}
