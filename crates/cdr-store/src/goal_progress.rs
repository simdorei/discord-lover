//! Durable progress payload and ownership handoff, committed before Discord I/O.
use crate::{Result, StoreError, schema::open_initialized};
use rusqlite::{Connection, TransactionBehavior, params};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct PendingProgress {
    pub job_id: Option<String>,
    pub thread: String,
    pub turn: String,
    pub channel: i64,
    pub content: String,
    pub last_error: String,
}

pub(crate) fn migrate_schema(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS codex_goal_progress (
        thread TEXT NOT NULL, turn TEXT NOT NULL, channel INTEGER NOT NULL,
        content TEXT NOT NULL, last_error TEXT NOT NULL DEFAULT '',
        PRIMARY KEY(thread,turn));",
    )?;
    if !c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('codex_goal_progress')
        WHERE name='job_id')",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        c.execute_batch("ALTER TABLE codex_goal_progress ADD COLUMN job_id TEXT;")?;
    }
    Ok(())
}

pub(crate) fn schema_current(c: &Connection) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('codex_goal_progress') WHERE name='job_id')",
        [],
        |r| r.get(0),
    )?)
}

pub fn stage(
    path: &Path,
    job_id: &str,
    turn: &str,
    generation: i64,
    content: &str,
) -> Result<Option<PendingProgress>> {
    stage_inner(path, job_id, turn, generation, content, None)
}

pub fn stage_owned(
    path: &Path,
    expected: &crate::queue::StoredQueueJob,
    content: &str,
) -> Result<Option<PendingProgress>> {
    let turn = expected
        .turn_id
        .as_deref()
        .ok_or_else(|| StoreError::InvalidQueueState("goal progress owner has no turn".into()))?;
    stage_inner(
        path,
        &expected.job_id,
        turn,
        expected.app_server_generation,
        content,
        Some(expected),
    )
}

fn stage_inner(
    path: &Path,
    job_id: &str,
    turn: &str,
    generation: i64,
    content: &str,
    expected_owner: Option<&crate::queue::StoredQueueJob>,
) -> Result<Option<PendingProgress>> {
    let mut c = open_initialized(path)?;
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let job = crate::queue::select_job(&tx, job_id)?;
    if let Some(expected) = expected_owner {
        let owners: i64 = tx.query_row(
            "SELECT COUNT(*) FROM codex_turn_queue WHERE target_thread_id=? AND state='running'",
            [&expected.target_thread_id],
            |row| row.get(0),
        )?;
        if expected != &job || owners != 1 {
            return Err(StoreError::InvalidQueueState(
                "goal progress ownership changed".into(),
            ));
        }
    }
    crate::dead_generation::ensure_target_available(&tx, &job.target_thread_id)?;
    if job.state != crate::queue::QueueJobState::Running
        || job.turn_id.as_deref() != Some(turn)
        || job.app_server_generation != generation
    {
        return Err(StoreError::InvalidQueueState(
            "goal progress ownership changed".into(),
        ));
    }
    let pending = if content.is_empty() {
        None
    } else {
        tx.execute(
            "INSERT OR IGNORE INTO codex_goal_progress(thread,turn,channel,content,job_id)
            VALUES(?,?,?,?,?)",
            params![job.target_thread_id, turn, job.channel_id, content, job_id],
        )?;
        let pending = tx.query_row(
            "SELECT thread,turn,channel,content,last_error,job_id
            FROM codex_goal_progress WHERE thread=? AND turn=?",
            params![job.target_thread_id, turn],
            read,
        )?;
        if pending.channel != job.channel_id
            || pending.content != content
            || pending.job_id.as_deref() != Some(job_id)
        {
            return Err(StoreError::InvalidQueueState(
                "goal progress payload conflict".into(),
            ));
        }
        Some(pending)
    };
    // Keep the exact completed turn bot-owned even after the next turn attaches.
    crate::mirror::record_job_origin(&tx, &job)?;
    tx.execute(
        "INSERT OR IGNORE INTO codex_session_mirror_events
        (event_digest,codex_thread_id,created_at) VALUES(?,?,?)",
        params![
            crate::mirror::turn_origin_marker(&job.target_thread_id, turn),
            job.target_thread_id,
            job.updated_at
        ],
    )?;
    tx.execute(
        "UPDATE codex_turn_queue SET goal_waiting=1 WHERE job_id=?",
        [job_id],
    )?;
    if !crate::async_resolution::retain_terminal_journal_in(&tx, &job.target_thread_id, turn)? {
        tx.execute(
            "DELETE FROM codex_observed_completions WHERE thread_id=? AND turn_id=?",
            params![job.target_thread_id, turn],
        )?;
    }
    tx.execute(
        "DELETE FROM codex_observed_final_answers WHERE thread_id=? AND turn_id=?",
        params![job.target_thread_id, turn],
    )?;
    tx.commit()?;
    Ok(pending)
}

fn read(r: &rusqlite::Row<'_>) -> rusqlite::Result<PendingProgress> {
    Ok(PendingProgress {
        thread: r.get(0)?,
        turn: r.get(1)?,
        channel: r.get(2)?,
        content: r.get(3)?,
        last_error: r.get(4)?,
        job_id: r.get(5)?,
    })
}

pub fn pending(path: &Path) -> Result<Vec<PendingProgress>> {
    let c = open_initialized(path)?;
    let mut q = c.prepare(
        "SELECT thread,turn,channel,content,last_error,job_id FROM cdr_live_codex_goal_progress ORDER BY rowid",
    )?;
    Ok(q.query_map([], read)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn record_error(path: &Path, p: &PendingProgress, error: &str) -> Result<()> {
    let bounded: String = error.chars().take(1000).collect();
    open_initialized(path)?.execute(
        "UPDATE codex_goal_progress SET last_error=? WHERE thread=? AND turn=?",
        params![bounded, p.thread, p.turn],
    )?;
    Ok(())
}

pub fn complete(path: &Path, p: &PendingProgress) -> Result<()> {
    open_initialized(path)?.execute(
        "DELETE FROM codex_goal_progress WHERE thread=? AND turn=?",
        params![p.thread, p.turn],
    )?;
    Ok(())
}

pub fn has_pending_job(path: &Path, job: &str, thread: &str) -> Result<bool> {
    let connection = open_initialized(path)?;
    has_pending_job_in(&connection, job, thread)
}

pub(crate) fn has_pending_job_in(connection: &Connection, job: &str, thread: &str) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM codex_goal_progress
        WHERE job_id=?1 OR (job_id IS NULL AND thread=?2))",
        params![job, thread],
        |r| r.get(0),
    )?)
}
