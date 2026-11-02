use crate::{Result, schema::open_initialized};
use rusqlite::params;
use std::path::Path;

/// A changed owner is not a reconnect to the original writer. Preserve identity,
/// but explicitly expire only answers that have never been dispatched.
pub fn retire_old_owner(path: &Path, runtime: &str, generation: i64) -> Result<usize> {
    open_initialized(path)?.execute("UPDATE cdr_async_question_inbox SET state='expired' WHERE id IN (SELECT id FROM cdr_live_cdr_async_question_inbox) AND ((runtime_id!=? OR generation!=?) AND state='waiting')",params![runtime,generation])?;
    Ok(open_initialized(path)?.execute("UPDATE cdr_async_questions SET state='expired',error='original question connection changed; no answer sent',updated_at=unixepoch() WHERE id IN (SELECT id FROM cdr_live_cdr_async_questions) AND ((runtime_id!=? OR generation!=?) AND state IN ('observed','open'))",params![runtime,generation])?)
}

pub fn supersede(
    path: &Path,
    runtime: &str,
    generation: i64,
    thread: &str,
    turn: &str,
) -> Result<usize> {
    Ok(open_initialized(path)?.execute("UPDATE cdr_async_questions SET state='expired',error='a newer turn superseded this question; no answer sent',updated_at=unixepoch() WHERE id IN (SELECT id FROM cdr_live_cdr_async_questions) AND (runtime_id=? AND generation=? AND thread_id=? AND turn_id!=? AND state IN ('observed','open'))",params![runtime,generation,thread,turn])?)
}

/// Trim old terminal payloads, never occurrence IDs or unresolved dispatch state.
/// Tombstones reject delayed replay instead of recreating a selectable question.
pub fn compact_terminal(path: &Path, now: f64) -> Result<usize> {
    if !now.is_finite() {
        return Err(super::invalid("invalid retention clock"));
    }
    open_initialized(path)?.execute("UPDATE cdr_async_question_inbox SET body='{\"index\":0,\"title\":\"\",\"options\":[]}' WHERE id IN (SELECT id FROM cdr_live_cdr_async_question_inbox) AND (state='expired' AND created_at<?)",[now-30.0*86400.0])?;
    Ok(open_initialized(path)?.execute(r#"UPDATE cdr_async_questions SET body='{"index":0,"title":"","options":[]}',error='terminal question tombstone'
        WHERE id IN (SELECT id FROM cdr_live_cdr_async_questions)
        AND state IN ('submitted','rejected','closed_unknown','unsupported','expired')
        AND updated_at<? AND body!='{"index":0,"title":"","options":[]}'"#,[now-30.0*86400.0])?)
}
