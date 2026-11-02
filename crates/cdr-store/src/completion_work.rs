//! Bounded metadata discovery for the completion worker. No schema or claim changes.
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::{Result, schema::open_initialized};

#[cfg(test)]
mod current_tests;
mod payload;
mod round;
pub use payload::{Payload, load};
pub use round::{MetadataReadRound, PageRead, read_round};

pub const PAGE_SIZE: usize = 32;
pub const MAX_METADATA_BYTES: usize = 4096;
pub const MAX_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    Observed,
    Queue,
    AsyncOrphan,
    Commentary,
    Goal,
    StartFailure,
    Question,
    Final,
}

impl Source {
    pub const ALL: [Self; 8] = [
        Self::Observed,
        Self::Queue,
        Self::AsyncOrphan,
        Self::Commentary,
        Self::Goal,
        Self::StartFailure,
        Self::Question,
        Self::Final,
    ];

    #[must_use]
    pub const fn is_state(self) -> bool {
        matches!(self, Self::Observed | Self::Queue | Self::AsyncOrphan)
    }

    fn select(self) -> &'static str {
        match self {
            Self::Observed => {
                "SELECT 0 stamp,rowid ordinal,'' sort_id,thread_id id,
                thread_id target,turn_id turn,0 channel,length(CAST(payload AS BLOB)) bytes
                FROM cdr_live_codex_observed_completions"
            }
            Self::Queue => {
                "SELECT DISTINCT 0 stamp,0 ordinal,target_thread_id sort_id,
                target_thread_id id,target_thread_id target,'' turn,0 channel,0 bytes
                FROM cdr_live_codex_turn_queue WHERE state<>'quarantined'"
            }
            Self::AsyncOrphan => {
                "SELECT DISTINCT 0 stamp,0 ordinal,o.thread_id sort_id,
                o.thread_id id,o.thread_id target,'' turn,0 channel,0 bytes
                FROM cdr_live_cdr_async_execution_obligations o WHERE o.execution_state='unresolved'
                AND NOT EXISTS(SELECT 1 FROM cdr_live_codex_turn_queue q WHERE q.job_id=o.origin_job_id)
                AND NOT EXISTS(SELECT 1 FROM cdr_live_codex_turn_queue q
                    WHERE q.target_thread_id=o.thread_id AND q.state<>'pending')"
            }
            Self::Commentary => {
                "SELECT 0 stamp,sequence ordinal,'' sort_id,
                CAST(sequence AS TEXT) id,target_thread_id target,turn_id turn,channel_id channel,
                length(CAST(text AS BLOB)) bytes,job_id job FROM cdr_live_codex_commentary_outbox"
            }
            Self::Goal => {
                "SELECT 0 stamp,rowid ordinal,'' sort_id,thread id,
                thread target,turn,channel,length(CAST(content AS BLOB)) bytes
                FROM cdr_live_codex_goal_progress"
            }
            Self::StartFailure => {
                "SELECT created_at stamp,0 ordinal,job_id sort_id,job_id id,
                target_thread_id target,'' turn,channel_id channel,
                length(CAST(content AS BLOB)) bytes FROM cdr_live_codex_reserve_start_notices"
            }
            Self::Question => {
                "SELECT created_at stamp,CAST(json_extract(body,'$.index') AS INTEGER) ordinal,
                id sort_id,id,thread_id target,turn_id turn,channel_id channel,
                length(CAST(body AS BLOB)) bytes FROM cdr_live_cdr_async_questions
                WHERE state='observed' AND runtime_id=(SELECT runtime FROM scope)
                AND generation=(SELECT generation FROM scope)"
            }
            Self::Final => {
                "SELECT created_at stamp,0 ordinal,delivery_id sort_id,delivery_id id,
                target_thread_id target,turn_id turn,channel_id channel,
                length(CAST(content AS BLOB)) bytes,job_id job FROM cdr_live_codex_delivery_outbox"
            }
        }
    }

    fn held_receipt_match(self) -> &'static str {
        match self {
            Self::Observed | Self::Queue | Self::AsyncOrphan => "0",
            Self::Final => "r.channel=h.channel AND r.domain='completion/v1' AND r.logical=h.id",
            Self::StartFailure => {
                "r.channel=h.channel AND r.domain='reserve/start-failure/v1' AND r.logical=h.id"
            }
            Self::Goal => {
                "r.channel=h.channel AND r.domain='completion/goal-progress/v1' AND
                r.logical=length(CAST(h.target AS BLOB))||':'||h.target||';'||
                    length(CAST(h.turn AS BLOB))||':'||h.turn||';'"
            }
            Self::Commentary => {
                "r.channel=h.channel AND r.domain='completion/commentary/v1' AND
                instr(r.logical,length(CAST(h.target AS BLOB))||':'||h.target||';'||
                    length(CAST(h.turn AS BLOB))||':'||h.turn||';')=1"
            }
            Self::Question => {
                "r.channel=h.channel AND (
                (r.domain IN ('async-question-v1','async-question-body-v1') AND r.logical=h.id)
                OR (r.domain='async-question-item-text-v1' AND EXISTS(
                    SELECT 1 FROM cdr_async_questions q WHERE q.id=h.id AND
                    r.logical=json_array(q.thread_id,q.turn_id,q.item_id))))"
            }
        }
    }

    fn query(self) -> String {
        self.query_for_channel(false)
    }

    fn query_for_channel(self, channel_scoped: bool) -> String {
        let lane = if self.is_state() { "target" } else { "channel" };
        let input = if channel_scoped && self == Self::Final {
            format!("{} WHERE channel_id=?6", self.select())
        } else {
            self.select().to_owned()
        };
        // Match first_reply::pending before electing a channel head. A request
        // whose initial reply is unconfirmed retains its output, but cannot
        // prevent independently acknowledged requests from being delivered.
        let first_reply_ready = "COALESCE((SELECT confirmation_delivered
            FROM discord_ingress_journal WHERE owner_kind='prompt' AND owner_id=h.job
            AND kind IN ('message','interaction') ORDER BY created_at,ingress_id LIMIT 1),1)<>0";
        let eligible = match self {
            // A held progress receipt belongs to its original turn. Keep that
            // turn's progress, but elect the next independent acknowledged job.
            Self::Commentary => format!(
                "{first_reply_ready} AND NOT EXISTS(SELECT 1 FROM unavailable r WHERE {})",
                self.held_receipt_match()
            ),
            Self::Final => format!(
                "({first_reply_ready} OR EXISTS(SELECT 1 FROM cdr_final_recovery WHERE delivery_id=h.id))
                AND NOT EXISTS(SELECT 1 FROM codex_commentary_outbox p JOIN unavailable r
                    ON r.channel=p.channel_id AND r.domain='completion/commentary/v1'
                    AND instr(r.logical,length(CAST(p.target_thread_id AS BLOB))||':'||p.target_thread_id||';'||
                        length(CAST(p.turn_id AS BLOB))||':'||p.turn_id||';')=1
                    WHERE p.job_id=h.job)"
            ),
            _ => "1".to_owned(),
        };
        // One head per source/lane. Different domains are NOT ordered by invented timestamps.
        format!(
            "WITH scope AS (SELECT ?1 runtime,?2 generation), source_input AS ({}),
             ranked AS (SELECT *,row_number() OVER
               (PARTITION BY {lane} ORDER BY stamp,ordinal,sort_id) lane_rank FROM source_input h WHERE {eligible}),
             unavailable AS (SELECT
                json_extract(CASE WHEN json_valid(receipt_key) THEN receipt_key ELSE '[]' END,'$[0]') channel,
                json_extract(CASE WHEN json_valid(receipt_key) THEN receipt_key ELSE '[]' END,'$[1]') domain,
                json_extract(CASE WHEN json_valid(receipt_key) THEN receipt_key ELSE '[]' END,'$[2]') logical
                FROM codex_delivery_receipts WHERE message_id IS NULL AND (retryable=0 OR blocked_reason IS NOT NULL)),
             heads AS (SELECT * FROM ranked WHERE lane_rank=1 AND
               length(CAST(sort_id AS BLOB))+length(CAST(id AS BLOB))+
               length(CAST(target AS BLOB))+length(CAST(turn AS BLOB))<={MAX_METADATA_BYTES}),
             candidates AS (SELECT * FROM heads h WHERE NOT EXISTS(
                SELECT 1 FROM unavailable r WHERE {}))",
            input,self.held_receipt_match()
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Position {
    stamp: f64,
    ordinal: i64,
    id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub source: Source,
    pub id: String,
    pub target: String,
    pub turn: String,
    pub channel: i64,
    pub bytes: usize,
    pub position: Position,
}

impl Entry {
    fn read(source: Source, r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            source,
            position: Position {
                stamp: r.get(0)?,
                ordinal: r.get(1)?,
                id: r.get(2)?,
            },
            id: r.get(3)?,
            target: r.get(4)?,
            turn: r.get(5)?,
            channel: r.get(6)?,
            bytes: usize::try_from(r.get::<_, i64>(7)?).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    7,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })?,
        })
    }

    #[must_use]
    pub fn same_identity(&self, other: &Self) -> bool {
        self.source == other.source
            && self.id == other.id
            && self.target == other.target
            && self.turn == other.turn
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cursor {
    after: Option<Position>,
    upper: Option<Position>,
    pub finished: bool,
}

#[derive(Debug, PartialEq)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub oversized_identity: bool,
    pub held_receipt_heads: i64,
}

/// Fixed high-water per pass; wakeups never rewind an unfinished pass.
pub fn page(
    path: &Path,
    source: Source,
    cursor: &mut Cursor,
    runtime: &str,
    generation: i64,
) -> Result<Page> {
    let mut db = open_initialized(path)?;
    let tx = db.transaction()?;
    page_in(&tx, source, cursor, runtime, generation)
}

fn page_in(
    tx: &Connection,
    source: Source,
    cursor: &mut Cursor,
    runtime: &str,
    generation: i64,
) -> Result<Page> {
    let query = source.query();
    if cursor.upper.is_none() {
        cursor.upper = tx
            .query_row(
                &format!(
                    "{query} SELECT stamp,ordinal,sort_id FROM candidates
                ORDER BY stamp DESC,ordinal DESC,sort_id DESC LIMIT 1"
                ),
                params![runtime, generation],
                |r| {
                    Ok(Position {
                        stamp: r.get(0)?,
                        ordinal: r.get(1)?,
                        id: r.get(2)?,
                    })
                },
            )
            .optional()?;
    }
    let oversized_identity: bool = tx.query_row(
        &format!(
            "{query} SELECT EXISTS(SELECT 1 FROM source_input WHERE
            length(CAST(sort_id AS BLOB))+length(CAST(id AS BLOB))+
            length(CAST(target AS BLOB))+length(CAST(turn AS BLOB))>{MAX_METADATA_BYTES})"
        ),
        params![runtime, generation],
        |r| r.get(0),
    )?;
    let receipt_heads = if source == Source::Commentary {
        "source_input"
    } else {
        "heads"
    };
    let held_receipt_heads: i64 = tx.query_row(
        &format!(
            "{query} SELECT COUNT(DISTINCT channel) FROM {receipt_heads} h WHERE EXISTS(
            SELECT 1 FROM unavailable r WHERE {})",
            source.held_receipt_match()
        ),
        params![runtime, generation],
        |r| r.get(0),
    )?;
    let Some(upper) = &cursor.upper else {
        cursor.finished = true;
        return Ok(Page {
            entries: Vec::new(),
            oversized_identity,
            held_receipt_heads,
        });
    };
    let first = Position {
        stamp: 0.0,
        ordinal: 0,
        id: String::new(),
    };
    let after = cursor.after.as_ref().unwrap_or(&first);
    let entries = tx
        .prepare(&format!(
            "{query}
        SELECT stamp,ordinal,sort_id,id,target,turn,channel,bytes FROM candidates
        WHERE (?3=0 OR (stamp,ordinal,sort_id)>(?4,?5,?6))
          AND (stamp,ordinal,sort_id)<=(?7,?8,?9)
        ORDER BY stamp,ordinal,sort_id LIMIT {PAGE_SIZE}"
        ))?
        .query_map(
            params![
                runtime,
                generation,
                cursor.after.is_some(),
                after.stamp,
                after.ordinal,
                after.id,
                upper.stamp,
                upper.ordinal,
                upper.id
            ],
            |r| Entry::read(source, r),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    cursor.finished = entries.len() < PAGE_SIZE;
    if let Some(last) = entries.last() {
        cursor.after = Some(last.position.clone());
    }
    Ok(Page {
        entries,
        oversized_identity,
        held_receipt_heads,
    })
}

/// Wake only existing source heads for a freshly processed target. This is not
/// delivery authority: payload loading and the original receipt guards recheck it.
pub fn heads_for_target(
    path: &Path,
    target: &str,
    runtime: &str,
    generation: i64,
) -> Result<Vec<Entry>> {
    let mut db = open_initialized(path)?;
    let tx = db.transaction()?;
    let mut heads = Vec::new();
    for source in Source::ALL.into_iter().filter(|source| !source.is_state()) {
        let head = tx
            .query_row(
                &format!(
                    "{} SELECT stamp,ordinal,sort_id,id,target,turn,channel,bytes FROM candidates
                WHERE target=?3 ORDER BY stamp,ordinal,sort_id LIMIT 1",
                    source.query()
                ),
                params![runtime, generation, target],
                |r| Entry::read(source, r),
            )
            .optional()?;
        if let Some(head) = head {
            heads.push(head);
        }
    }
    Ok(heads)
}

fn current(
    db: &Connection,
    entry: &Entry,
    runtime: &str,
    generation: i64,
) -> Result<Option<Entry>> {
    if entry.source == Source::Final {
        // Keep every row in this channel before ranking. Filtering by delivery
        // identity here would incorrectly promote a later row past its head.
        return Ok(db
            .query_row(
                &format!(
                    "{} SELECT stamp,ordinal,sort_id,id,target,turn,channel,bytes FROM candidates
                 WHERE id=?3 AND target=?4 AND turn=?5",
                    entry.source.query_for_channel(true)
                ),
                params![
                    runtime,
                    generation,
                    entry.id,
                    entry.target,
                    entry.turn,
                    entry.channel
                ],
                |r| Entry::read(entry.source, r),
            )
            .optional()?);
    }
    Ok(db
        .query_row(
            &format!(
                "{} SELECT stamp,ordinal,sort_id,id,target,turn,channel,bytes FROM candidates
            WHERE id=?3 AND target=?4 AND turn=?5",
                entry.source.query()
            ),
            params![runtime, generation, entry.id, entry.target, entry.turn],
            |r| Entry::read(entry.source, r),
        )
        .optional()?)
}
