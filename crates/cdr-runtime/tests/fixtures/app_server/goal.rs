use super::{Logging, Path, Result, env, json, method, reply, serve, turn, wait_file};

fn advance(rollout: &Path) -> Result {
    std::fs::write(
        rollout,
        format!(
            "{}\n",
            json!({"timestamp":"1","type":"event_msg","payload":{"type":"task_complete","turn_id":"T2","last_agent_message":"goal final"}})
        ),
    )?;
    turn("thread", "T2", false)?;
    turn("thread", "T2", true)
}

pub(super) fn run() -> Result {
    let log = env("GOAL_TEST_LOG");
    let rollout = env("GOAL_TEST_ROLLOUT");
    let mut goal_done = false;
    let mut early_goal = false;
    serve(Path::new(&log), Logging::PlainMethods, |request| {
        let result = match method(&request) {
            "initialize" => json!({"userAgent":"goal-test"}),
            "test/early-goal" => {
                early_goal = true;
                json!({})
            }
            "test/advance-goal" => {
                goal_done = true;
                advance(Path::new(&rollout))?;
                json!({})
            }
            "thread/goal/get" if early_goal => {
                early_goal = false;
                goal_done = true;
                advance(Path::new(&rollout))?;
                wait_file(Path::new(&format!("{rollout}.release")), 5, true)?;
                json!({"goal":{"threadId":"thread","status":"active"}})
            }
            "thread/goal/get" => {
                json!({"goal":{"threadId":"thread","status":if goal_done || Path::new(&format!("{rollout}.goal-complete")).exists() {"complete"} else {"active"}}})
            }
            "thread/read" | "thread/turns/list" => {
                let turns = if Path::new(&format!("{rollout}.omit-history")).exists() {
                    vec![]
                } else if goal_done {
                    vec![("T1", "progress"), ("T2", "goal final")]
                } else {
                    vec![("T1", "progress")]
                };
                if method(&request) == "thread/turns/list" {
                    assert_eq!(request["params"]["itemsView"], "notLoaded");
                    json!({"data":turns.into_iter().rev().map(|(id,_)|
                        json!({"id":id,"status":"completed","items":[],"itemsStatus":"notLoaded"})
                    ).collect::<Vec<_>>(),"nextCursor":null})
                } else {
                    json!({"thread":{"id":"thread","turns":turns.into_iter().map(|(id,text)| json!({"id":id,"status":"completed","items":[{"type":"agentMessage","text":text,"phase":"final_answer"}]})).collect::<Vec<_>>()}})
                }
            }
            _ => json!({}),
        };
        reply(&request, &result)
    })
}
