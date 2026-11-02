use std::path::Path;

use rusqlite::{Connection, Row};

use super::{QueueJobState, StoredQueueJob, is_quarantine_encoding};
use crate::schema::open_initialized;
use crate::{Result, StoreError};

pub(crate) const COLUMNS: &str = "job_id, target_thread_id, channel_id, owner_user_id, \
    discord_message_id, app_server_generation, execution_generation, prompt, queued, ack_sent, state, \
    attempt_count, turn_id, baseline_turn_ids, last_error, created_at, updated_at, goal_waiting, turn_observation_generation";

struct RawQueueJob {
    job_id: String,
    target_thread_id: String,
    channel_id: i64,
    owner_user_id: Option<i64>,
    discord_message_id: Option<i64>,
    app_server_generation: i64,
    execution_generation: Option<i64>,
    prompt: String,
    queued: i64,
    ack_sent: i64,
    state: String,
    attempt_count: i64,
    turn_id: Option<String>,
    baseline_turn_ids: String,
    last_error: String,
    created_at: f64,
    updated_at: f64,
    goal_waiting: i64,
    turn_observation_generation: Option<i64>,
}

impl RawQueueJob {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            job_id: row.get(0)?,
            target_thread_id: row.get(1)?,
            channel_id: row.get(2)?,
            owner_user_id: row.get(3)?,
            discord_message_id: row.get(4)?,
            app_server_generation: row.get(5)?,
            execution_generation: row.get(6)?,
            prompt: row.get(7)?,
            queued: row.get(8)?,
            ack_sent: row.get(9)?,
            state: row.get(10)?,
            attempt_count: row.get(11)?,
            turn_id: row.get(12)?,
            baseline_turn_ids: row.get(13)?,
            last_error: row.get(14)?,
            created_at: row.get(15)?,
            updated_at: row.get(16)?,
            goal_waiting: row.get(17)?,
            turn_observation_generation: row.get(18)?,
        })
    }

    fn decode(self) -> Result<StoredQueueJob> {
        let state = match self.state.as_str() {
            "pending" => QueueJobState::Pending,
            "starting" => QueueJobState::Starting,
            "running"
                if is_quarantine_encoding(
                    &self.state,
                    self.turn_id.as_deref(),
                    &self.last_error,
                ) =>
            {
                QueueJobState::Quarantined
            }
            "running" => QueueJobState::Running,
            _ => return Err(StoreError::InvalidQueueState(self.state)),
        };
        let baseline_turn_ids =
            match serde_json::from_str::<serde_json::Value>(&self.baseline_turn_ids)? {
                serde_json::Value::Array(values) => values
                    .into_iter()
                    .map(|value| match value {
                        serde_json::Value::String(text) => text,
                        other => other.to_string(),
                    })
                    .collect(),
                _ => Vec::new(),
            };
        Ok(StoredQueueJob {
            job_id: self.job_id,
            target_thread_id: self.target_thread_id,
            channel_id: self.channel_id,
            owner_user_id: self.owner_user_id,
            discord_message_id: self.discord_message_id,
            app_server_generation: self.app_server_generation,
            execution_generation: self.execution_generation,
            turn_observation_generation: self.turn_observation_generation,
            prompt: self.prompt,
            queued: self.queued != 0,
            ack_sent: self.ack_sent != 0,
            state,
            attempt_count: self.attempt_count,
            turn_id: self.turn_id,
            baseline_turn_ids,
            last_error: self.last_error,
            created_at: self.created_at,
            updated_at: self.updated_at,
            goal_waiting: self.goal_waiting != 0,
        })
    }
}

pub(crate) fn select_job(connection: &Connection, job_id: &str) -> Result<StoredQueueJob> {
    let sql = format!("SELECT {COLUMNS} FROM codex_turn_queue WHERE job_id = ?");
    let raw = connection
        .query_row(&sql, [job_id], RawQueueJob::read)
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => StoreError::QueueJobNotFound(job_id.into()),
            other => other.into(),
        })?;
    raw.decode()
}

pub(crate) fn all_jobs(connection: &Connection) -> Result<Vec<StoredQueueJob>> {
    query_jobs(
        connection,
        &format!("SELECT {COLUMNS} FROM cdr_live_codex_turn_queue ORDER BY created_at, job_id"),
    )
}

fn query_jobs(connection: &Connection, sql: &str) -> Result<Vec<StoredQueueJob>> {
    let mut statement = connection.prepare(sql)?;
    let raw = statement
        .query_map([], RawQueueJob::read)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter().map(RawQueueJob::decode).collect()
}

pub fn list(path: &Path) -> Result<Vec<StoredQueueJob>> {
    let connection = open_initialized(path)?;
    all_jobs(&connection)
}

pub fn list_filtered(
    path: &Path,
    target_thread_id: Option<&str>,
    generation: Option<i64>,
) -> Result<Vec<StoredQueueJob>> {
    let connection = open_initialized(path)?;
    let (sql, values): (String, Vec<rusqlite::types::Value>) = match (target_thread_id, generation)
    {
        (Some(target), Some(value)) => (
            format!(
                "SELECT {COLUMNS} FROM cdr_live_codex_turn_queue WHERE target_thread_id = ? AND app_server_generation = ? ORDER BY created_at, job_id"
            ),
            vec![target.to_owned().into(), value.into()],
        ),
        (Some(target), None) => (
            format!(
                "SELECT {COLUMNS} FROM cdr_live_codex_turn_queue WHERE target_thread_id = ? ORDER BY created_at, job_id"
            ),
            vec![target.to_owned().into()],
        ),
        (None, Some(value)) => (
            format!(
                "SELECT {COLUMNS} FROM cdr_live_codex_turn_queue WHERE app_server_generation = ? ORDER BY created_at, job_id"
            ),
            vec![value.into()],
        ),
        (None, None) => return all_jobs(&connection),
    };
    let mut statement = connection.prepare(&sql)?;
    let raw = statement
        .query_map(rusqlite::params_from_iter(values.iter()), RawQueueJob::read)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter().map(RawQueueJob::decode).collect()
}
