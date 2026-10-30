//! The pending answer owns an immutable preparation and job snapshot.
//! Rechecked by the resident's actual-write mutation journal, not just the UI.
use super::{Question, invalid, read, validate_mapping};
use crate::{Result, schema::open_initialized};
use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Serialize, Deserialize)]
struct Seal {
    identity: Value,
}

fn identity(db: &Connection, q: &Question) -> Result<Value> {
    validate_mapping(db, q)?;
    let job_id = q.reply_job_id.as_deref().unwrap_or(&q.origin_job_id);
    let job = crate::queue::select_job(db, job_id)?;
    if job.target_thread_id != q.thread_id
        || job.channel_id != q.channel_id
        || job.owner_user_id != Some(q.owner_user_id)
    {
        return Err(invalid("async reply exact job ownership changed"));
    }
    if q.reply_job_id.is_some() {
        let count: i64 = db.query_row(
            "SELECT COUNT(*) FROM codex_turn_queue WHERE target_thread_id=?",
            [&q.thread_id],
            |r| r.get(0),
        )?;
        if count != 1
            || job.state != crate::queue::QueueJobState::Quarantined
            || job.app_server_generation != q.generation
        {
            return Err(invalid(
                "async reply reservation changed or successor appeared",
            ));
        }
    } else if !super::ownership::running_matches(&job, q)
        || !super::ownership::running_owned_in(db, q)?
    {
        return Err(invalid("async steer original turn changed"));
    }
    // JSON float parsing is not an exact IEEE-754 snapshot without float_roundtrip.
    // Persist the timestamp bits so a valid one-shot claim cannot reject itself.
    let mut job_identity = serde_json::to_value(&job)?;
    job_identity["created_at"] = json!(job.created_at.to_bits());
    job_identity["updated_at"] = json!(job.updated_at.to_bits());
    let mut identity = json!({
        "question": [&q.runtime_id, &q.thread_id, &q.turn_id, &q.item_id, &q.origin_job_id],
        "generation":q.generation,"channel":q.channel_id,"actor":q.owner_user_id,
        "message":q.message_id,"chosen":q.chosen,"body":q.body,
        "reply_job_id":q.reply_job_id,"job":job_identity,
    });
    super::bind_text(&mut identity, q);
    Ok(identity)
}

pub(super) fn seal_in(db: &Connection, id: &str) -> Result<()> {
    let q = read(db, id)?;
    let seal = Seal {
        identity: identity(db, &q)?,
    };
    db.execute(
        "UPDATE cdr_async_questions SET preparation_json=? WHERE id=? AND state='dispatching'",
        rusqlite::params![serde_json::to_string(&seal)?, id],
    )?;
    Ok(())
}

pub(super) fn verify_identity_in(db: &Connection, q: &Question) -> Result<()> {
    let encoded: Option<String> = db.query_row(
        "SELECT preparation_json FROM cdr_async_questions WHERE id=?",
        [&q.id],
        |r| r.get(0),
    )?;
    let encoded = encoded
        .ok_or_else(|| invalid("legacy async dispatch has no confirmed preparation; held"))?;
    let seal: Seal = serde_json::from_str(&encoded)?;
    if identity(db, q)? != seal.identity {
        return Err(invalid("async reply exact identity changed after claim"));
    }
    Ok(())
}

pub fn validate_dispatch_guards(path: &Path, thread: &str) -> Result<()> {
    let mut db = open_initialized(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Deferred)?;
    crate::async_resolution::guard_mutation_in(&tx, thread)?;
    // An ACK settles the answer, not its current execution. Include only live
    // obligations; historical submitted questions must not strand later work.
    let ids = tx
        .prepare(
            "SELECT q.id FROM cdr_async_questions q WHERE q.thread_id=? AND
            (q.state='dispatching' OR EXISTS(
                SELECT 1 FROM cdr_async_unsettled_obligations o
                WHERE o.thread_id=q.thread_id AND o.question_id=q.id))",
        )?
        .query_map([thread], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let q = read(&tx, &id)?;
        // Only the general CURRENT-execution guard recognizes this certificate.
        // Original answer dispatch/confirmation still calls verify_identity_in.
        if crate::async_resolution::certified_successor_in(&tx, thread, &id)? {
            // A handoff replaces the original turn, not common target safety.
            validate_mapping(&tx, &q)?;
            if !super::ownership::sole_nonpending_owner_in(&tx, &q)? {
                return Err(invalid(
                    "async successor current execution ownership is not unique",
                ));
            }
            continue;
        }
        verify_identity_in(&tx, &q)?;
    }
    Ok(())
}
