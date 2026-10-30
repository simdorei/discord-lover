//! Immutable async-question occurrences; dispatch claims never expire into retries.
use crate::{Result, StoreError, schema::open_initialized};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

mod answer;
mod dispatch;
pub(crate) use answer::bind_text;
pub use answer::{
    Answer, FREE_TEXT_COMPONENT, FREE_TEXT_FORMAT_VERSION, MAX_ANSWER_CHARS, answer_prompt_with,
    chosen_answer,
};
mod guard;
pub use guard::validate_dispatch_guards;
mod inbox;
pub(crate) use inbox::reconcile_job_in;
mod observe;
mod ownership;
mod retention;
mod schema;
pub use dispatch::{
    Claim, DispatchMode, begin_dispatch, confirm_dispatch, record_error, reject_definite,
    reject_usage_limit,
};
pub use inbox::{reconcile_observations, record_observation};
pub use observe::{NewQuestion, observe};
pub use retention::{compact_terminal, retire_old_owner, supersede};
pub(crate) use schema::{migrate_schema, schema_current};

pub const DELIVERY_DOMAIN: &str = "async-question-v1";
pub const ANSWER_PREFIX: &str = "The user answered exactly this earlier async question through Discord. Apply this selection only to this question; other questions remain unanswered.";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuestionBody {
    pub index: usize,
    #[serde(default)]
    pub source_text: String,
    pub title: String,
    pub options: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Question {
    pub id: String,
    pub runtime_id: String,
    pub generation: i64,
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub origin_job_id: String,
    pub channel_id: i64,
    pub owner_user_id: i64,
    pub body: QuestionBody,
    pub state: String,
    pub message_id: Option<String>,
    pub chosen: Option<usize>,
    pub chosen_text: Option<String>,
    pub reply_job_id: Option<String>,
    pub error: String,
}

pub fn occurrence_id(thread: &str, turn: &str, item: &str, index: usize) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
        thread, turn, item, index,
    ))?)))
}

pub fn answer_prompt(q: &Question, option: usize) -> Result<String> {
    answer_prompt_with(q, &Answer::Choice(option))
}

pub fn get(path: &Path, id: &str) -> Result<Question> {
    read(&open_initialized(path)?, id)
}

pub fn target_dispatch_held(path: &Path, thread: &str) -> Result<bool> {
    if crate::async_resolution::admission_held(path, thread)? {
        return Ok(true);
    }

    dispatch_held_in(&open_initialized(path)?, thread)
}

pub(crate) fn dispatch_held_in(db: &Connection, thread: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM cdr_async_questions WHERE thread_id=? AND dispatch_mode='start' AND state='dispatching')",[thread],|r|r.get(0))?)
}

pub(crate) fn read(db: &Connection, id: &str) -> Result<Question> {
    let (mut q, payload): (Question, String) = db.query_row(
        "SELECT runtime_id,generation,thread_id,turn_id,item_id,origin_job_id,channel_id,owner_user_id,body,state,message_id,chosen,reply_job_id,error,answer_text FROM cdr_async_questions WHERE id=?",
        [id], |r| Ok((Question {
            id:id.into(), runtime_id:r.get(0)?, generation:r.get(1)?, thread_id:r.get(2)?,
            turn_id:r.get(3)?, item_id:r.get(4)?, origin_job_id:r.get(5)?, channel_id:r.get(6)?,
            owner_user_id:r.get(7)?, body:QuestionBody{index:0,source_text:String::new(),title:String::new(),options:vec![]},
            state:r.get(9)?, message_id:r.get(10)?, chosen:r.get::<_,Option<u16>>(11)?.map(usize::from), reply_job_id:r.get(12)?, error:r.get(13)?, chosen_text:r.get(14)?,
        }, r.get(8)?)))?;
    q.body = serde_json::from_str(&payload)?;
    Ok(q)
}

pub fn pending(path: &Path, runtime: &str) -> Result<Vec<Question>> {
    let db = open_initialized(path)?;
    let ids = db.prepare("SELECT id FROM cdr_async_questions WHERE runtime_id=? AND state='observed' ORDER BY created_at,json_extract(body,'$.index'),id LIMIT 100")?
        .query_map([runtime], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ids.iter().map(|id| read(&db, id)).collect()
}

pub fn owner_confirmed(path: &Path, q: &Question) -> Result<bool> {
    let mut db = open_initialized(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let confirmed: bool = tx.query_row(
        "SELECT owner_confirmed FROM cdr_async_questions WHERE id=?",
        [&q.id],
        |r| r.get(0),
    )?;
    if confirmed {
        return Ok(true);
    }
    // A generationless Final outbox is not proof of a current question owner.
    // Completion preserves verified questions before removing the exact job.
    let current = read(&tx, &q.id)?;
    let confirmed = ownership::running_owned_in(&tx, &current)?;
    if confirmed {
        tx.execute(
            "UPDATE cdr_async_questions SET owner_confirmed=1 WHERE id=?",
            [&q.id],
        )?;
    }
    tx.commit()?;
    Ok(confirmed)
}

pub fn require_current_mapping(path: &Path, q: &Question) -> Result<()> {
    validate_mapping(&open_initialized(path)?, q)
}

pub fn receipt_key(q: &Question) -> Result<String> {
    Ok(serde_json::to_string(&(
        q.channel_id,
        DELIVERY_DOMAIN,
        &q.id,
        0,
    ))?)
}

/// Bind only the message ID from the actual durable Discord HTTP receipt.
pub fn bind_receipt(path: &Path, id: &str, interactive: bool) -> Result<()> {
    let db = open_initialized(path)?;
    let q = read(&db, id)?;
    if !owner_confirmed(path, &q)? {
        return Err(invalid("question original turn ownership is not confirmed"));
    }
    let receipt: Option<String> = db
        .query_row(
            "SELECT message_id FROM codex_delivery_receipts WHERE receipt_key=?",
            [receipt_key(&q)?],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let message = receipt.ok_or_else(|| invalid("question message receipt is not confirmed"))?;
    db.execute(
        "UPDATE cdr_async_questions SET message_id=?,state=? WHERE id=? AND state='observed'",
        params![
            message,
            if interactive { "open" } else { "unsupported" },
            id
        ],
    )?;
    Ok(())
}

pub(crate) fn validate_mapping(db: &Connection, q: &Question) -> Result<()> {
    let matches: bool = db.query_row("SELECT COUNT(*)=1 AND MIN(codex_thread_id)=?2 FROM mirror_threads WHERE discord_thread_id=?1",
        params![q.channel_id,q.thread_id], |r|r.get(0))?;
    let fenced: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM cdr_cleanup_fences WHERE channel_id=?1) OR EXISTS(SELECT 1 FROM codex_dead_generation_holds WHERE target_thread_id=?2) OR EXISTS(SELECT 1 FROM codex_archive_fences WHERE target_thread_id=?2)",params![q.channel_id,q.thread_id],|r|r.get(0))?;
    if !matches || fenced {
        return Err(invalid(
            "question mapping changed or target is fenced; no answer sent",
        ));
    }
    Ok(())
}

fn invalid(message: &str) -> StoreError {
    StoreError::Integrity(message.into())
}
