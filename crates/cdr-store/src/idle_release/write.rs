use super::{
    Connection, Intent, MAX_DIAGNOSTIC_CHARS, MAX_UNRESOLVED, Path, Result, StoreError,
    bot_idle_on, open_initialized, params, select,
};
use rusqlite::TransactionBehavior;

pub(crate) fn before_enqueue(db: &Connection, thread: &str) -> Result<()> {
    if let Some(intent) = select(db, thread)? {
        match intent.state.as_str() {
            "Candidate" => update(db, &intent, "Settled", "CancelledBeforeSend")?,
            "Settled" | "AwaitUnload" => {}
            _ => {
                return Err(StoreError::Integrity(format!(
                    "idle release {} requires review for thread {thread}; new prompt was not enqueued: {}",
                    intent.state, intent.detail
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn stage_candidate(
    db: &Connection,
    job: &crate::queue::StoredQueueJob,
    owner: &str,
) -> Result<()> {
    if !bot_idle_on(db, &job.target_thread_id)? {
        return Ok(());
    }
    let existing = select(db, &job.target_thread_id)?;
    if existing.as_ref().is_some_and(|i| i.state != "Settled") {
        return Ok(());
    }
    // Final is already staged in this transaction. Capacity only defers release.
    let count: i64 = db.query_row(
        "SELECT COUNT(*) FROM cdr_live_cdr_idle_release WHERE state!='Settled'",
        [],
        |r| r.get(0),
    )?;
    if count >= MAX_UNRESOLVED {
        eprintln!("idle_release_deferred reason=capacity limit={MAX_UNRESOLVED}");
        return Ok(());
    }
    db.execute("DELETE FROM cdr_idle_release WHERE state='Settled' AND thread_id NOT IN (SELECT thread_id FROM cdr_idle_release WHERE state='Settled' ORDER BY rowid DESC LIMIT 32)", [])?;
    db.execute("INSERT OR REPLACE INTO cdr_idle_release(intent_id,owner_id,generation,thread_id,turn_id,job_id,revision,state) VALUES(?,?,?,?,?,?,1,'Candidate')",
        params![uuid::Uuid::new_v4().to_string(), owner, job.app_server_generation,
            job.target_thread_id, job.turn_id, job.job_id])?;
    Ok(())
}

/// Called under resident target admission protection. Only an unsent candidate
/// is cancellable; only a persisted ACK may authorize one real resume.
pub fn before_mutation(
    path: &Path,
    owner: &str,
    generation: i64,
    thread: &str,
) -> Result<Option<Intent>> {
    let mut db = open_initialized(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if crate::archived_cleanup::blocked_in(&tx, None, Some(thread))?
        || crate::archived_cleanup::record_retired_in(&tx, "cdr_idle_release", thread)?
    {
        return Err(StoreError::Integrity(
            "archived idle work cannot be resumed".into(),
        ));
    }
    let Some(mut intent) = select(&tx, thread)? else {
        return Ok(None);
    };
    match intent.state.as_str() {
        "Settled" => Ok(None),
        "Candidate" => {
            update(&tx, &intent, "Settled", "CancelledBeforeSend")?;
            tx.commit()?;
            Ok(None)
        }
        "AwaitUnload" if intent.owner_id == owner && intent.generation == generation => {
            update(
                &tx,
                &intent,
                "Resubscribing",
                "real resume required before next mutation",
            )?;
            intent.revision += 1;
            intent.state = "Resubscribing".into();
            tx.commit()?;
            Ok(Some(intent))
        }
        _ => Err(StoreError::Integrity(format!(
            "idle release {} requires review for thread {}; no automatic resume/start: {}",
            intent.state, thread, intent.detail
        ))),
    }
}

fn allowed(from: &str, to: &str, reason: &str) -> bool {
    matches!(
        (from, to),
        ("Candidate", "Dispatching" | "Candidate")
            | ("Dispatching", "AwaitUnload")
            | ("Dispatching" | "Resubscribing", "Unknown")
    ) || (to == "Settled"
        && match reason {
            "CancelledBeforeSend" => matches!(from, "Candidate" | "Dispatching"),
            "UnloadedConfirmed" => from == "AwaitUnload",
            "SupersededByConfirmedResubscribe" => from == "Resubscribing",
            "OldServerExited" => from != "Settled",
            _ => false,
        })
        || (from == "Resubscribing" && to == "AwaitUnload" && reason == "ResumeCancelledBeforeSend")
}

fn update(db: &Connection, old: &Intent, state: &str, detail: &str) -> Result<()> {
    let bounded: String = detail.chars().take(MAX_DIAGNOSTIC_CHARS).collect();
    let changed = db.execute(
        "UPDATE cdr_idle_release SET state=?,detail=?,revision=revision+1
        WHERE intent_id=? AND owner_id=? AND generation=? AND thread_id=? AND turn_id=?
        AND job_id=? AND revision=? AND state=?",
        params![
            state,
            bounded,
            old.intent_id,
            old.owner_id,
            old.generation,
            old.thread_id,
            old.turn_id,
            old.job_id,
            old.revision,
            old.state
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::Integrity(
            "idle release compare-and-set lost".into(),
        ));
    }
    Ok(())
}

pub fn transition(path: &Path, old: &Intent, state: &str, detail: &str) -> Result<Intent> {
    if !allowed(&old.state, state, detail) {
        return Err(StoreError::Integrity(format!(
            "invalid idle release transition {} -> {state}",
            old.state
        )));
    }
    let mut db = open_initialized(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    update(&tx, old, state, detail)?;
    let next = select(&tx, &old.thread_id)?.expect("updated intent");
    tx.commit()?;
    Ok(next)
}

/// Only the caller holding the exact old child handle may provide this evidence.
/// Runtime UUID or generation changes alone must never call this method.
pub fn settle_exited_owner(path: &Path, owner: &str, generation: i64) -> Result<()> {
    open_initialized(path)?.execute(
        "UPDATE cdr_idle_release SET state='Settled',
        detail='OldServerExited', revision=revision+1
        WHERE owner_id=? AND generation=? AND state!='Settled'",
        params![owner, generation],
    )?;
    Ok(())
}

/// Caller owns an IMMEDIATE cleanup transaction. Cancelling an unsent candidate
/// and publishing the cleanup fence commit together; stale workers fail their CAS.
pub(crate) fn before_cleanup(db: &Connection, thread: &str) -> Result<()> {
    if let Some(intent) = select(db, thread)?
        && intent.state == "Candidate"
    {
        update(db, &intent, "Settled", "CancelledBeforeSend")?;
    }
    Ok(())
}
