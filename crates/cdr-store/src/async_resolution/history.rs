//! Exact historical candidates, never notification or execution authority.
use super::{FORMAT_VERSION, Obligation, held, read_in};
use crate::{
    Result,
    async_question::{Question, QuestionBody},
};
use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path};
mod settlement;
pub(crate) use settlement::settle_stopped_execution_in;
pub use settlement::{
    TerminalHistorySnapshot, capture_terminal_history_snapshot, settle_terminal_history,
};

pub struct HistorySnapshot {
    thread: String,
    rows: Vec<Obligation>,
    mapping: Vec<(String, i64)>,
}

impl HistorySnapshot {
    #[must_use]
    pub fn turn_ids(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|r| r.turn_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    #[must_use]
    pub fn obligation_count(&self) -> usize {
        self.rows.len()
    }
}

fn snapshot_in(db: &Connection, thread: &str) -> Result<HistorySnapshot> {
    let rows = read_in(db, thread)?;
    let mapping = db
        .prepare(
            "SELECT codex_thread_id,discord_thread_id FROM mirror_threads
        WHERE codex_thread_id=?1 OR discord_thread_id IN
        (SELECT channel_id FROM cdr_async_execution_obligations WHERE thread_id=?1)
        ORDER BY codex_thread_id,discord_thread_id",
        )?
        .query_map([thread], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(HistorySnapshot {
        thread: thread.into(),
        rows,
        mapping,
    })
}

pub fn capture_history_snapshot(path: &Path, thread: &str) -> Result<Option<HistorySnapshot>> {
    let db = crate::schema::open_initialized(path)?;
    let tx = db.unchecked_transaction()?;
    let snapshot = snapshot_in(&tx, thread)?;
    if snapshot.rows.is_empty() {
        return Ok(None);
    }
    for row in &snapshot.rows {
        let _ = original_question(row)?;
    }
    Ok(Some(snapshot))
}

pub(super) fn original_question(row: &Obligation) -> Result<Question> {
    let invalid = || {
        held(
            &row.thread_id,
            "historical review requires the complete immutable original question seal",
        )
    };
    if row.version != FORMAT_VERSION || row.revision < 0 {
        return Err(invalid());
    }
    let claim: Value = serde_json::from_str(&row.claim)?;
    let seal: Value = serde_json::from_str(row.original_seal.as_deref().ok_or_else(invalid)?)?;
    let string = |key| {
        claim
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(invalid)
    };
    let integer = |key| claim.get(key).and_then(Value::as_i64).ok_or_else(invalid);
    let body: QuestionBody = serde_json::from_str(&string("body")?)?;
    let chosen = claim
        .get("chosen")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok());
    let q = Question {
        id: string("id")?,
        runtime_id: string("runtime_id")?,
        generation: integer("generation")?,
        thread_id: string("thread_id")?,
        turn_id: string("turn_id")?,
        item_id: string("item_id")?,
        origin_job_id: string("origin_job_id")?,
        channel_id: integer("channel_id")?,
        owner_user_id: integer("owner_user_id")?,
        body,
        state: "dispatching".into(),
        message_id: Some(string("message_id")?),
        chosen,
        chosen_text: claim
            .get("answer_text")
            .and_then(Value::as_str)
            .map(str::to_owned),
        reply_job_id: None,
        error: row.original_error.clone(),
    };
    let job = &seal["identity"]["job"];
    let mut identity = json!({"question":[&q.runtime_id,&q.thread_id,&q.turn_id,&q.item_id,&q.origin_job_id],
        "generation":q.generation,"channel":q.channel_id,"actor":q.owner_user_id,
        "message":q.message_id,"chosen":q.chosen,"body":q.body,"reply_job_id":null,"job":job});
    crate::async_question::bind_text(&mut identity, &q);
    let answer = crate::async_question::chosen_answer(&q).map_err(|_| invalid())?;
    if q.id != row.question_id
        || q.thread_id != row.thread_id
        || q.turn_id != row.turn_id
        || q.origin_job_id != row.origin_job_id
        || claim["dispatch_mode"] != "steer"
        || q.generation < 0
        || q.channel_id <= 0
        || q.owner_user_id <= 0
        || seal["identity"] != identity
        || job["job_id"] != q.origin_job_id
        || job["target_thread_id"] != q.thread_id
        || job["channel_id"] != q.channel_id
        || job["owner_user_id"] != q.owner_user_id
        || job["turn_id"] != q.turn_id
        || crate::async_question::answer_prompt_with(&q, &answer)?.len() > 65_536
    {
        return Err(invalid());
    }
    Ok(q)
}

fn hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn fingerprint(snapshot: &HistorySnapshot) -> Result<String> {
    Ok(serde_json::to_string(&(&snapshot.rows, &snapshot.mapping))?)
}

fn exact_input(turn: &Value, q: &Question) -> Result<(Option<Value>, bool)> {
    let items = turn
        .get("items")
        .and_then(Value::as_array)
        .filter(|items| items.len() <= 1024)
        .ok_or_else(|| {
            held(
                &q.thread_id,
                "historical turn items are missing or exceed the bound",
            )
        })?;
    let mut item_ids = BTreeSet::new();
    for item in items {
        if let Some(id) = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            && !item_ids.insert(id)
        {
            return Err(held(&q.thread_id, "historical item identity is duplicated"));
        }
    }
    let prompt =
        crate::async_question::answer_prompt_with(q, &crate::async_question::chosen_answer(q)?)?;
    let mut matched = None;
    let mut conflict = false;
    for item in items {
        if item["type"] != "userMessage" {
            continue;
        }
        let Some(content) = item
            .get("content")
            .and_then(Value::as_array)
            .filter(|v| v.len() == 1)
        else {
            continue;
        };
        if content[0]["type"] != "text" {
            continue;
        }
        let Some(text) = content[0]["text"].as_str() else {
            continue;
        };
        if text == prompt {
            if item
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(|id| id.is_empty() || id.len() > 512)
            {
                return Err(held(
                    &q.thread_id,
                    "historical accepted input identity is missing",
                ));
            }
            if matched.is_some() {
                conflict = true;
            }
            matched = Some(item.clone());
        } else if let Some((prefix, payload)) = text.split_once('\n')
            && prefix == crate::async_question::ANSWER_PREFIX
            && let Ok(other) = serde_json::from_str::<Value>(payload)
            && other["thread_id"] == q.thread_id
            && other["original_turn_id"] == q.turn_id
            && other["question_item_id"] == q.item_id
            && other["question_index"] == q.body.index
        {
            conflict = true;
        }
    }
    Ok((if conflict { None } else { matched }, conflict))
}

fn candidate(
    row: &Obligation,
    turn: &Value,
    history_hash: &str,
    observer: &str,
    generation: i64,
) -> Result<Value> {
    let q = original_question(row)?;
    let (input, conflict) = exact_input(turn, &q)?;
    let terminal = match turn.get("status").and_then(Value::as_str) {
        Some("completed" | "failed" | "interrupted") => {
            turn.get("status").cloned().unwrap_or(Value::Null)
        }
        Some("inProgress") => Value::Null,
        _ => {
            return Err(held(
                &q.thread_id,
                "historical original turn status is not typed",
            ));
        }
    };
    let facts = json!({"question_id":row.question_id,"revision":row.revision,
        "claim_sha256":row.claim_sha256,"thread_id":row.thread_id,"original_turn_id":row.turn_id,
        "input":input,"answer_conflict":conflict,"terminal_status":terminal});
    let review_key = hash(&facts.to_string());
    Ok(
        json!({"version":1,"source":"historical_read_candidate_v1","observer":observer,
        "generation":generation,"question_id":row.question_id,"revision":row.revision,
        "claim_sha256":row.claim_sha256,"thread_id":row.thread_id,"original_turn_id":row.turn_id,
        "history_sha256":history_hash,"original_turn_sha256":hash(&turn.to_string()),
        "answer_input_id":input.as_ref().and_then(|v|v.get("id")),
        "matching_input":input,"answer_conflict":conflict,"terminal_status":terminal,
        "review_key":review_key,"execution_authority":false}),
    )
}

fn semantic_facts(evidence: &Value) -> Value {
    json!({"question_id":evidence["question_id"],"revision":evidence["revision"],
        "claim_sha256":evidence["claim_sha256"],"thread_id":evidence["thread_id"],
        "original_turn_id":evidence["original_turn_id"],"input":evidence["matching_input"],
        "answer_conflict":evidence["answer_conflict"],"terminal_status":evidence["terminal_status"]})
}

fn is_digest(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The semantic key is only an index. Receipt confirmation requires the exact
/// stored bytes, provenance and answer facts, including after INSERT triggers.
/// Older reader/history identities may differ without changing those facts.
fn valid_candidate_in(
    db: &Connection,
    row: &Obligation,
    key: &str,
    expected: &Value,
) -> Result<bool> {
    let invalid = || {
        held(
            &row.thread_id,
            "stored historical candidate does not preserve the exact verified answer facts",
        )
    };
    let mut statement=db.prepare(
        "SELECT evidence_sha256,CASE WHEN length(CAST(evidence_text AS BLOB))<=131072
         THEN evidence_text END FROM cdr_async_terminal_candidates
         WHERE question_id=? AND revision=? AND kind='unverified'
         AND CASE WHEN json_valid(evidence_text) THEN json_extract(evidence_text,'$.review_key') END=?
         LIMIT 9",
    )?;
    let rows = statement
        .query_map(params![row.question_id, row.revision, key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.len() > 8 {
        return Err(invalid());
    }
    let present = !rows.is_empty();
    for (digest, raw) in rows {
        let raw = raw.ok_or_else(invalid)?;
        if hash(&raw) != digest {
            return Err(invalid());
        }
        let stored: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
        if stored["version"] != 1
            || stored["source"] != "historical_read_candidate_v1"
            || stored["execution_authority"] != false
            || stored
                .get("observer")
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty() || s.len() > 256)
            || stored
                .get("generation")
                .and_then(Value::as_i64)
                .is_none_or(|n| n < 0)
            || !is_digest(&stored["history_sha256"])
            || !is_digest(&stored["original_turn_sha256"])
            || stored.get("matching_input").is_none()
            || stored.get("terminal_status").is_none()
            || stored.get("answer_input_id") != expected.get("answer_input_id")
            || semantic_facts(&stored) != semantic_facts(expected)
            || hash(&semantic_facts(&stored).to_string()) != key
        {
            return Err(invalid());
        }
    }
    Ok(present)
}

/// Save candidate provenance and receipt state atomically. Never settle execution.
pub fn retain_history_candidate(
    path: &Path,
    expected: &HistorySnapshot,
    history: &Value,
    observer: &str,
    generation: i64,
) -> Result<usize> {
    let encoded = history.to_string();
    if encoded.len() > 1_048_576
        || observer.is_empty()
        || observer.len() > 256
        || generation < 0
        || history["threadId"] != expected.thread
        || history["truncated"] == true
    {
        return Err(held(
            &expected.thread,
            "invalid or oversized historical observation",
        ));
    }
    let turns = history
        .get("turns")
        .and_then(Value::as_array)
        .filter(|v| v.len() <= 128)
        .ok_or_else(|| held(&expected.thread, "historical turn page is invalid"))?;
    let mut ids = BTreeSet::new();
    for turn in turns {
        let id = turn
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| held(&expected.thread, "historical turn identity is missing"))?;
        if !ids.insert(id) {
            return Err(held(
                &expected.thread,
                "historical turn identity is duplicated",
            ));
        }
    }
    let mut db = crate::schema::open_initialized(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if fingerprint(&snapshot_in(&tx, &expected.thread)?)? != fingerprint(expected)? {
        return Err(held(
            &expected.thread,
            "historical review snapshot changed while reading",
        ));
    }
    let history_hash = hash(&encoded);
    let mut retained = 0;
    for row in &expected.rows {
        let Some(turn) = turns.iter().find(|t| t["id"] == row.turn_id) else {
            continue;
        };
        let evidence = candidate(row, turn, &history_hash, observer, generation)?;
        let key = evidence["review_key"]
            .as_str()
            .ok_or_else(|| held(&expected.thread, "missing review key"))?;
        if !valid_candidate_in(&tx, row, key, &evidence)? {
            super::candidates::retain(&tx, row, "unverified", &evidence.to_string())?;
        }
        if !valid_candidate_in(&tx, row, key, &evidence)? {
            return Err(held(
                &expected.thread,
                "historical candidate storage bound reached; receipt unchanged",
            ));
        }
        if !evidence["answer_input_id"].is_null() {
            tx.execute("UPDATE cdr_async_execution_obligations SET answer_state='exact_history_confirmed',
                receipt_turn=turn_id WHERE question_id=? AND revision=? AND answer_state='unresolved'",
                params![row.question_id,row.revision])?;
        }
        retained += 1;
    }
    tx.commit()?;
    Ok(retained)
}
