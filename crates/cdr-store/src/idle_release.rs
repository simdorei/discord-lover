//! Bounded, durable ownership of optional idle subscription release.
use crate::{Result, StoreError, schema::open_initialized};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

mod write;
pub(crate) use write::stage_candidate;
pub(crate) use write::{before_cleanup, before_enqueue};
pub use write::{before_mutation, settle_exited_owner, transition};

pub const MAX_UNRESOLVED: i64 = 128;
pub const MAX_DIAGNOSTIC_CHARS: usize = 512;
const COLUMNS: &str =
    "intent_id,owner_id,generation,thread_id,turn_id,job_id,revision,state,detail";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    pub intent_id: String,
    pub owner_id: String,
    pub generation: i64,
    pub thread_id: String,
    pub turn_id: String,
    pub job_id: String,
    pub revision: i64,
    pub state: String,
    pub detail: String,
}

impl Intent {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            intent_id: row.get(0)?,
            owner_id: row.get(1)?,
            generation: row.get(2)?,
            thread_id: row.get(3)?,
            turn_id: row.get(4)?,
            job_id: row.get(5)?,
            revision: row.get(6)?,
            state: row.get(7)?,
            detail: row.get(8)?,
        })
    }
}

pub(crate) fn migrate_schema(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS cdr_idle_release (
        intent_id TEXT NOT NULL UNIQUE, owner_id TEXT NOT NULL, generation INTEGER NOT NULL,
        thread_id TEXT PRIMARY KEY, turn_id TEXT NOT NULL, job_id TEXT NOT NULL,
        revision INTEGER NOT NULL, state TEXT NOT NULL CHECK(state IN
        ('Candidate','Dispatching','AwaitUnload','Resubscribing','Unknown','Settled')),
        detail TEXT NOT NULL DEFAULT '');",
    )?;
    Ok(())
}

pub(crate) fn schema_current(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='cdr_idle_release' AND type='table')",
        [],
        |r| r.get(0),
    )?)
}

fn select(db: &Connection, thread: &str) -> Result<Option<Intent>> {
    Ok(db
        .query_row(
            &format!("SELECT {COLUMNS} FROM cdr_idle_release WHERE thread_id=?"),
            [thread],
            Intent::read,
        )
        .optional()?)
}

pub fn get(path: &Path, thread: &str) -> Result<Option<Intent>> {
    select(&open_initialized(path)?, thread)
}

pub fn pending(path: &Path) -> Result<Vec<Intent>> {
    let db = open_initialized(path)?;
    let mut stmt = db.prepare(&format!(
        "SELECT {COLUMNS} FROM cdr_live_cdr_idle_release WHERE state!='Settled' ORDER BY thread_id LIMIT {MAX_UNRESOLVED}"
    ))?;
    Ok(stmt
        .query_map([], Intent::read)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// No missing row/cache is used as proof of the live server's idle state.
/// This checks only the durable bot-side disqualifiers, including quarantine.
pub fn bot_idle(path: &Path, thread: &str) -> Result<bool> {
    bot_idle_on(&open_initialized(path)?, thread)
}

fn bot_idle_on(db: &Connection, thread: &str) -> Result<bool> {
    if crate::async_resolution::held_in(db, thread)? {
        return Ok(false);
    }

    Ok(db.query_row(
        "SELECT
        NOT EXISTS(SELECT 1 FROM codex_turn_queue WHERE target_thread_id=?1)
        AND NOT EXISTS(SELECT 1 FROM cdr_async_questions WHERE thread_id=?1
            AND state NOT IN ('submitted','rejected','closed_unknown')
            AND NOT (state='expired' AND chosen IS NULL AND dispatch_mode IS NULL
                AND reply_job_id IS NULL AND accepted_turn_id IS NULL AND preparation_json IS NULL))
        AND NOT EXISTS(SELECT 1 FROM cdr_async_question_inbox WHERE thread_id=?1 AND state!='expired')
        AND NOT EXISTS(SELECT 1 FROM codex_dead_generation_holds WHERE target_thread_id=?1)
        AND NOT EXISTS(SELECT 1 FROM codex_archive_fences WHERE target_thread_id=?1)
        AND NOT EXISTS(SELECT 1 FROM cdr_cleanup_fences WHERE target_thread_id=?1)",
        [thread],
        |r| r.get(0),
    )?)
}

pub fn verify(path: &Path, expected: &Intent, require_idle: bool) -> Result<()> {
    let db = open_initialized(path)?;
    verify_in(&db, expected, require_idle)
}

/// Token, bot obligations and observation coverage are read in one snapshot.
pub fn verify_with_observations(path: &Path, expected: &Intent, require_idle: bool) -> Result<()> {
    let mut db = open_initialized(path)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
    verify_in(&tx, expected, require_idle)?;
    let scope = crate::observation_gap::Scope {
        owner_id: expected.owner_id.clone(),
        generation: expected.generation,
    };
    if require_idle && !crate::observation_gap::scope_verified_in(&tx, &scope, 0)? {
        return Err(StoreError::Integrity(
            "idle unverified: durable observation range remains".into(),
        ));
    }
    tx.commit()?;
    Ok(())
}

fn verify_in(db: &Connection, expected: &Intent, require_idle: bool) -> Result<()> {
    let current = select(db, &expected.thread_id)?;
    if current
        .as_ref()
        .is_none_or(|i| !same_revision(i, expected) || i.state != expected.state)
    {
        return Err(StoreError::Integrity(
            "idle release identity/state changed".into(),
        ));
    }
    if require_idle && !bot_idle_on(db, &expected.thread_id)? {
        return Err(StoreError::Integrity(
            "idle release deferred: unresolved bot work".into(),
        ));
    }
    Ok(())
}

fn same_revision(a: &Intent, b: &Intent) -> bool {
    a.intent_id == b.intent_id
        && a.owner_id == b.owner_id
        && a.generation == b.generation
        && a.thread_id == b.thread_id
        && a.turn_id == b.turn_id
        && a.job_id == b.job_id
        && a.revision == b.revision
}
