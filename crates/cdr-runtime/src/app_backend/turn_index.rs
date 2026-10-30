//! Queue baselines need every turn identity/status, not the conversation bodies.
use super::{
    AppServerTurnBackend, BackendFailure, TurnBackend, TurnRecord, definite,
    validate_thread_identity,
};
use cdr_app_server::{ResidentAppServer, outcomes::parse_thread_turn_states, requests::AppRequest};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tokio::time::Instant;

impl AppServerTurnBackend {
    pub(super) async fn read_turn_index(
        &self,
        thread: &str,
    ) -> Result<Vec<TurnRecord>, BackendFailure> {
        Ok(read(
            &self.server,
            thread,
            self.generation(),
            Instant::now() + self.history_read_timeout,
        )
        .await?
        .turns)
    }
}

pub(crate) struct TurnIndex {
    pub turns: Vec<TurnRecord>,
    pub latest_turn_id: Option<String>,
}

pub(crate) async fn read(
    server: &ResidentAppServer,
    thread: &str,
    generation: u64,
    deadline: Instant,
) -> Result<TurnIndex, BackendFailure> {
    let mut latest_turn_id = None;
    let metadata = index_request(
        server,
        "thread/read",
        json!({"threadId":thread,"includeTurns":false}),
        generation,
        deadline,
    )
    .await?;
    validate_thread_identity(&metadata, thread).map_err(BackendFailure::definite)?;
    let mut cursor = Value::Null;
    let mut cursors = BTreeSet::new();
    let mut turns = BTreeMap::new();
    let mut bytes = 0usize;
    for page_index in 0..1024 {
        let page = index_request(
            server,
            "thread/turns/list",
            json!({
                "threadId":thread,"itemsView":"notLoaded","limit":100,
                "sortDirection":"desc","cursor":cursor
            }),
            generation,
            deadline,
        )
        .await?;
        bytes = bytes.saturating_add(page.to_string().len());
        if bytes > 16 * 1024 * 1024
            || page
                .get("truncated")
                .is_some_and(|v| v != &Value::Bool(false))
        {
            return Err(BackendFailure::definite(
                "turn index is truncated or exceeds its byte bound; no task started",
            ));
        }
        let data = page
            .get("data")
            .and_then(Value::as_array)
            .filter(|v| v.len() <= 100)
            .ok_or_else(|| BackendFailure::definite("turn index page has no bounded turn array"))?;
        if page_index == 0 {
            latest_turn_id = data
                .first()
                .and_then(|turn| turn["id"].as_str())
                .map(str::to_owned);
        }
        let parsed =
            parse_thread_turn_states(&json!({"thread":{"id":thread,"turns":data}}), thread)
                .map_err(|e| BackendFailure::definite(e.to_string()))?;
        if parsed.len() != data.len() {
            return Err(BackendFailure::definite("duplicate turn index identity"));
        }
        for (id, turn) in parsed {
            if turns
                .insert(
                    id,
                    TurnRecord {
                        turn_id: turn.turn_id,
                        status: turn.status,
                    },
                )
                .is_some()
            {
                return Err(BackendFailure::definite(
                    "turn index changed during pagination",
                ));
            }
        }
        match page.get("nextCursor") {
            None | Some(Value::Null) => {
                if generation != server.generation() {
                    return Err(BackendFailure::definite("turn index connection changed"));
                }
                return Ok(TurnIndex {
                    turns: turns.into_values().collect(),
                    latest_turn_id,
                });
            }
            Some(Value::String(next))
                if !next.is_empty() && next.len() <= 2048 && !data.is_empty() =>
            {
                if !cursors.insert(next.clone()) {
                    return Err(BackendFailure::definite("turn index cursor repeated"));
                }
                cursor = json!(next);
            }
            _ => return Err(BackendFailure::definite("invalid turn index cursor")),
        }
    }
    Err(BackendFailure::definite(
        "turn index page bound exceeded; partial history is not a baseline",
    ))
}

async fn index_request(
    server: &ResidentAppServer,
    method: &'static str,
    params: Value,
    generation: u64,
    deadline: Instant,
) -> Result<Value, BackendFailure> {
    let timeout = deadline
        .checked_duration_since(Instant::now())
        .filter(|v| !v.is_zero())
        .ok_or_else(|| BackendFailure::definite("turn index read deadline exceeded"))?;
    server
        .execute(
            AppRequest {
                method,
                params,
                timeout,
            },
            Some(generation),
        )
        .await
        .map_err(|error| definite(&error))
}
