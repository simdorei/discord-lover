use super::{
    Logging, Path, Result, Value, Write, append, env, error, json, method, reply, serve, turn,
};

#[derive(Default)]
struct State {
    cwd: Value,
    turn: Option<Value>,
    persisted_cwd: Option<Value>,
    recovery: bool,
    drop_response: bool,
    hide_history: bool,
}

pub(super) fn run() -> Result {
    let log = env("CDR_ACTION_RPC_LOG");
    let persisted = std::env::var("CDR_NEW_STATE_FIXTURE").ok();
    let mut state = State::default();
    let mut stale_steer = false;
    serve(Path::new(&log), Logging::Requests, |request| {
        let params = &request["params"];
        let thread = params["threadId"].as_str().unwrap_or("");
        let result = match method(&request) {
            "initialize" => json!({"userAgent":"action-test/1"}),
            "test/new-recovery-mode" => {
                state.recovery = true;
                state.drop_response = params["drop_response"].as_bool().unwrap_or(false);
                state.hide_history = params["hide_history"].as_bool().unwrap_or(false);
                if state.turn.is_none() {
                    for line in std::fs::read_to_string(&log)?.lines() {
                        let value: Value = serde_json::from_str(line)?;
                        if value["method"] == "fixture/turn-accepted" {
                            state.turn = Some(value["turn"].clone());
                            state.cwd = value["cwd"].clone();
                        }
                    }
                }
                json!({})
            }
            "test/persist-new-in-other-project" => {
                state.persisted_cwd = Some(params["cwd"].clone());
                json!({})
            }
            "test/active-turn" | "test/finish-turn" => {
                turn(
                    thread,
                    params["turnId"].as_str().ok_or("missing turnId")?,
                    method(&request) == "test/finish-turn",
                )?;
                json!({})
            }
            "test/stale-next-steer" => {
                stale_steer = true;
                json!({})
            }
            "turn/steer" if stale_steer => {
                stale_steer = false;
                turn(thread, "current", true)?;
                turn(thread, "next", false)?;
                return error(
                    &request,
                    -32600,
                    "stale expectedTurnId: current; active turn is next",
                );
            }
            "thread/start" => {
                state.cwd = params["cwd"].clone();
                json!({"thread":{"id":"new-thread","turns":[]}})
            }
            "model/list" => {
                json!({"data":[{"model":"gpt-6-astra","displayName":"GPT-6-Astra","supportedReasoningEfforts":[{"reasoningEffort":"max"}]},{"model":"gpt-5.6-sol"},{"model":"model-a"}]})
            }
            "thread/read" | "thread/resume" if thread == "new-thread" => {
                if state.recovery || (persisted.is_some() && state.turn.is_some()) {
                    json!({"thread":{"id":"new-thread","cwd":state.cwd,"turns":if state.recovery && state.hide_history { vec![] } else { state.turn.iter().cloned().collect::<Vec<_>>() }}})
                } else {
                    return error(
                        &request,
                        -32600,
                        "no rollout found for thread id new-thread",
                    );
                }
            }
            "turn/start" if thread == "new-thread" => {
                let new_turn = json!({"id":"first-turn","status":"inProgress","items":[{"id":"first-user","type":"userMessage","content":params["input"]}]});
                if state.recovery {
                    state.turn = Some(new_turn.clone());
                    append(
                        Path::new(&log),
                        &json!({"method":"fixture/turn-accepted","turn":new_turn,"cwd":state.cwd}),
                    )?;
                }
                if let Some(path) = &persisted {
                    state.turn = Some(new_turn);
                    persist(path, &state, &params["input"])?;
                }
                if state.drop_response {
                    return Ok(());
                }
                json!({"turn":{"id":"first-turn","status":"inProgress"}})
            }
            "thread/resume" if thread == "thread-a" => {
                return error(
                    &request,
                    -32600,
                    "thread thread-a already has an active writer",
                );
            }
            "thread/read" | "thread/resume" => json!({"thread":{"id":thread,"turns":[]}}),
            "thread/turns/list" => turn_index(&state, thread),
            "turn/start" => json!({"turn":{"id":"existing-turn","status":"inProgress"}}),
            _ => json!({}),
        };
        fixture_reply(&request, &result)
    })
}

fn fixture_reply(request: &Value, result: &Value) -> Result {
    if let Ok(path) = std::env::var("CDR_STOP_TERMINAL_PATH")
        && let Ok(text) = std::fs::read_to_string(path)
    {
        let observed: Value = serde_json::from_str(&text)?;
        if request["params"]["threadId"] == observed["thread"]["id"] {
            let terminal = match method(request) {
                "thread/read" => Some(json!({"thread":observed["thread"]})),
                "thread/goal/get" => Some(json!({"goal":observed["goal"]})),
                "thread/turns/list" => {
                    let mut page = json!({"data":observed["turns"],"nextCursor":null});
                    if request["params"]["itemsView"] == "full" {
                        page["data"][0]["items"] = json!([{"text":"x".repeat(1_048_577)}]);
                    }
                    Some(page)
                }
                _ => None,
            };
            if let Some(terminal) = terminal {
                return reply(request, &terminal);
            }
        }
    }
    if method(request) == "turn/interrupt"
        && std::env::var_os("CDR_STOP_DROP_INTERRUPT_REPLY").is_some()
    {
        return Ok(());
    }
    emit_external_observation(request)?;
    reply(request, result)
}

// Out-of-band fixture observation, not a privileged control RPC.
fn emit_external_observation(request: &Value) -> Result {
    if method(request) == "thread/read"
        && let Ok(path) = std::env::var("CDR_STOP_NEXT_ACTIVE_PATH")
        && let Ok(text) = std::fs::read_to_string(&path)
    {
        let observation: Value = serde_json::from_str(&text)?;
        turn(
            observation["threadId"]
                .as_str()
                .ok_or("missing observed thread")?,
            observation["turnId"]
                .as_str()
                .ok_or("missing observed turn")?,
            false,
        )?;
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn persist(path: &str, state: &State, input: &Value) -> Result {
    let rollout = Path::new(path).parent().unwrap().join("new-rollout.jsonl");
    let records = [
        json!({"type":"session_meta","payload":{"id":"new-thread","cwd":state.cwd}}),
        json!({"type":"turn_context","payload":{"turn_id":"first-turn"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":input}}),
    ];
    let mut file = std::fs::File::create(&rollout)?;
    for value in records {
        serde_json::to_writer(&mut file, &value)?;
        writeln!(file)?;
    }
    let cwd = state
        .persisted_cwd
        .as_ref()
        .unwrap_or(&state.cwd)
        .as_str()
        .ok_or("missing new cwd")?;
    rusqlite::Connection::open(path)?.execute(
        "INSERT INTO threads VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        rusqlite::params![
            "new-thread",
            "new",
            cwd,
            50,
            rollout.to_string_lossy(),
            "model-a",
            "high",
            0,
            0,
            0,
            "app-server",
            "user"
        ],
    )?;
    Ok(())
}

fn turn_index(state: &State, thread: &str) -> Value {
    json!({"data":if thread == "new-thread" && !state.hide_history {
                state.turn.iter().cloned().collect::<Vec<_>>()
            } else { vec![] },"nextCursor":null})
}
