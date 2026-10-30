//! Strict wire fixture for async question ownership/chronology/uncertainty tests.
use super::{Logging, Path, Result, emit, env, error, json, method, reply, serve, turn};

pub(super) fn run() -> Result {
    let log = env("CDR_ACTION_RPC_LOG");
    let mut active = false;
    let mut latest = "original".to_owned();
    let mut reads = 0;
    let mut stale_on_second = false;
    let mut drop_reply = false;
    let mut goal = false;
    let mut goal_complete = false;
    let mut history_kind = String::new();
    serve(Path::new(&log), Logging::Requests, |request| {
        let params = &request["params"];
        let result = match method(&request) {
            "initialize" => json!({"userAgent":"async-question-test"}),
            "test/active-turn" => {
                active = true;
                turn("thread-b", "original", false)?;
                json!({})
            }
            "test/finish-turn" => {
                active = false;
                turn("thread-b", "original", true)?;
                json!({})
            }
            "test/stale-on-second-read" => {
                (stale_on_second, reads) = (true, 0);
                json!({})
            }
            "test/drop-reply" => {
                drop_reply = true;
                json!({})
            }
            "test/goal-on" => {
                goal = true;
                json!({})
            }
            "test/goal-complete" => {
                goal = true;
                goal_complete = true;
                json!({})
            }
            "test/history-kind" => {
                let kind = params["kind"].as_str().unwrap();
                kind.clone_into(&mut history_kind);
                json!({})
            }
            "test/complete-answer" => {
                assert_eq!(latest, "answer-turn");
                active = false;
                turn("thread-b", &latest, true)?;
                json!({})
            }
            "test/goal-next-question" => {
                active = true;
                latest = "goal-next".into();
                turn("thread-b", &latest, false)?;
                emit_question(&latest)?;
                json!({})
            }
            "test/question" => {
                emit_question(&latest)?;
                json!({})
            }
            "thread/turns/list" if params["itemsView"] == "notLoaded" => {
                advance_read(&mut latest, &mut reads, stale_on_second);
                turn_index(params, goal, &latest, active, &history_kind)
            }
            "thread/turns/list" => {
                turn_history(params, active, &mut latest, &mut reads, stale_on_second)
            }
            "thread/goal/get" => goal_snapshot(goal, goal_complete),
            "thread/read" if skip_read(params, &history_kind, reads) => return Ok(()),
            "thread/read" => history_snapshot(goal, &latest, active, &history_kind),
            "turn/steer" => {
                assert!(active);
                assert_eq!(params["threadId"], "thread-b");
                assert_eq!(params["expectedTurnId"], "original");
                if drop_reply {
                    return Ok(());
                }
                json!({"turnId":"original"})
            }
            "turn/start" => {
                begin_answer(params, &mut active, &mut latest);
                if drop_reply {
                    return Ok(());
                }
                json!({"turn":{"id":"answer-turn","status":"inProgress"}})
            }
            _ => {
                return error(
                    &request,
                    -32601,
                    "unsupported async question fixture method",
                );
            }
        };
        reply(&request, &result)
    })
}

fn begin_answer(params: &serde_json::Value, active: &mut bool, latest: &mut String) {
    assert!(!*active);
    assert_eq!(params["threadId"], "thread-b");
    *active = true;
    *latest = "answer-turn".into();
}

fn turn_history(
    params: &serde_json::Value,
    active: bool,
    latest: &mut String,
    reads: &mut usize,
    stale_on_second: bool,
) -> serde_json::Value {
    assert_eq!(params["threadId"], "thread-b");
    assert_eq!(params["sortDirection"], "desc");
    assert_eq!(params["limit"], 1);
    advance_read(latest, reads, stale_on_second);
    json!({"data":[{"id":latest,"status":if active {"inProgress"} else {"completed"},"items":[]}],"nextCursor":null})
}

fn goal_snapshot(goal: bool, complete: bool) -> serde_json::Value {
    if goal {
        json!({"goal":{"threadId":"thread-b","status":if complete {"complete"} else {"active"}}})
    } else {
        json!({"goal":null})
    }
}

fn thread_snapshot(goal: bool, latest: &str, active: bool) -> serde_json::Value {
    let items = if goal {
        vec![json!({"type":"agentMessage","text":"진행 시험","phase":"final_answer"})]
    } else {
        vec![]
    };
    let mut turns = vec![
        json!({"id":latest,"status":if active {"inProgress"} else {"completed"},"items":items}),
    ];
    if goal && latest != "original" {
        turns.insert(0, json!({"id":"original","status":"completed","items":[{"type":"agentMessage","text":"진행 시험","phase":"final_answer"}]}));
    }
    json!({"thread":{"id":"thread-b","turns":turns}})
}

fn history_snapshot(goal: bool, latest: &str, active: bool, kind: &str) -> serde_json::Value {
    let mut result = thread_snapshot(goal, latest, active);
    let turns = result["thread"]["turns"].as_array_mut().unwrap();
    match kind {
        "heavy-paged" => {
            for i in 0..200 {
                turns.insert(
                    i,
                    json!({"id":format!("older-{i}"),"status":"completed","items":[]}),
                );
            }
        }
        "prior" | "successor" => {
            turns.insert(
                0,
                json!({"id":"older-ui-turn","status":"completed","items":[]}),
            );
            if kind == "successor" {
                turns.push(json!({"id":"unobserved-successor","status":"completed","items":[]}));
            }
        }
        "duplicate" => turns.push(turns[0].clone()),
        "missing-original" => turns.retain(|turn| turn["id"] != "original"),
        "active" => turns.push(json!({"id":"other-active","status":"inProgress","items":[]})),
        _ => {}
    }
    if kind == "wrong-thread" {
        result["thread"]["id"] = json!("other-thread");
    } else if kind == "missing-turns" {
        result["thread"].as_object_mut().unwrap().remove("turns");
    }
    result
}

pub(super) fn emit_question(turn_id: &str) -> Result {
    emit(
        &json!({"method":"item/completed","params":{"threadId":"thread-b","turnId":turn_id,"item":{
            "id":"question-call","type":"agentMessage","phase":"final_answer","delivery":"async","text":"어느 프로젝트인가요?",
            "questions":[{"title":"프로젝트 A?","options":["허용","보류"]},{"title":"프로젝트 B?","options":["허용","보류"]}]
        }}}),
    )
}

fn turn_index(
    params: &serde_json::Value,
    goal: bool,
    latest: &str,
    active: bool,
    kind: &str,
) -> serde_json::Value {
    assert_eq!(params["threadId"], "thread-b");
    assert_eq!(params["sortDirection"], "desc");
    assert_eq!(params["limit"], 100);
    let snapshot = history_snapshot(goal, latest, active, kind);
    let offset = params["cursor"]
        .as_str()
        .and_then(|v| v.strip_prefix("page-"))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let turns = snapshot["thread"]["turns"].as_array();
    let data = turns.map(|turns| {
        turns.iter().rev().skip(offset).take(100).map(|turn|
        json!({"id":turn["id"],"status":turn["status"],"items":[],"itemsStatus":"notLoaded"})
    ).collect::<Vec<_>>()
    });
    let next = turns
        .filter(|turns| offset + 100 < turns.len())
        .map(|_| format!("page-{}", offset + 100));
    json!({"data":data,"nextCursor":next})
}

fn advance_read(latest: &mut String, reads: &mut usize, stale_on_second: bool) {
    *reads += 1;
    if stale_on_second && *reads >= 2 {
        *latest = "newer".into();
    }
}

fn skip_read(params: &serde_json::Value, kind: &str, reads: usize) -> bool {
    kind == "metadata-timeout"
        || (kind == "second-read-timeout" && reads > 0)
        || (kind == "heavy-paged" && params["includeTurns"] == true)
}
