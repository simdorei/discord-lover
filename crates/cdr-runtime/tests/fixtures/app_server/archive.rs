use super::{
    Duration, Logging, Path, Result, conflicting_identity, env, error, json, method, reply, serve,
    thread, turn, wait_file,
};

pub(super) fn run() -> Result {
    let scenario = env("ARCHIVE_SCENARIO");
    let log = env("ARCHIVE_LOG");
    let mut lists = 0;
    serve(Path::new(&log), Logging::Requests, |request| {
        let thread = thread(&request);
        let result = match method(&request) {
            "initialize" => json!({"userAgent":"archive-test/1"}),
            "thread/resume" => {
                if scenario == "descendant_writer" && thread == "child" {
                    return error(
                        &request,
                        -32600,
                        "thread child already has an active writer",
                    );
                }
                if scenario == "became_active" {
                    turn("thread-b", "late-turn", false)?;
                }
                let mut result = json!({"thread":{"id":thread,"status":{"type":if scenario == "became_active" {"active"} else {"idle"}},"turns":[]}});
                conflicting_identity(&mut result, &scenario, "resume", thread);
                result
            }
            "thread/read" => {
                let mut result = json!({"thread":{"id":thread,"status":{"type":if scenario == "active_without_event" {"active"} else {"idle"}},"turns":[]}});
                if scenario == "missing_status" {
                    result["thread"].as_object_mut().unwrap().remove("status");
                }
                if scenario == "wrong_read_identity" {
                    result["thread"]["id"] = json!("other");
                }
                conflicting_identity(&mut result, &scenario, "read", thread);
                result
            }
            "thread/list" => {
                lists += 1;
                if scenario == "stall_list" {
                    std::thread::sleep(Duration::from_secs(5));
                }
                let mut result = json!({"data":if scenario.starts_with("descendant_") {json!([{"id":"child"}])} else {json!([])},"nextCursor":null});
                if scenario == "scope_malformed" {
                    result = json!({"data":{}});
                }
                if scenario == "descendant_changed" && lists > 1 {
                    result["data"] = json!([]);
                }
                if scenario == "scope_cursor_repeat" {
                    result["nextCursor"] = json!("same");
                }
                if scenario == "scope_missing_cursor" {
                    result.as_object_mut().unwrap().remove("nextCursor");
                }
                if scenario == "scope_root" {
                    result["data"] = json!([{"id":"thread-b"}]);
                }
                if scenario == "writer_gate" && lists == 2 {
                    wait_file(Path::new(&format!("{log}.list_release")), 8, false)?;
                }
                result
            }
            "thread/archive" => {
                if scenario == "archive_writer_reject" {
                    return error(
                        &request,
                        -32600,
                        "thread thread-b already has an active writer",
                    );
                }
                if scenario != "no_persistence" {
                    if matches!(scenario.as_str(), "archive_gate" | "descendant_gate") {
                        wait_file(Path::new(&format!("{log}.release")), 8, false)?;
                    }
                    persist_archive(&scenario, thread)?;
                    if scenario == "stall_archive" {
                        std::thread::sleep(Duration::from_secs(5));
                    }
                    if scenario == "stall_archive_inner" {
                        std::thread::sleep(Duration::from_secs(12));
                    }
                    if scenario == "archive_disconnect" {
                        std::process::exit(0);
                    }
                }
                json!({})
            }
            _ => json!({}),
        };
        reply(&request, &result)?;
        if scenario == "writer_gate" && method(&request) == "thread/list" && lists == 2 {
            wait_file(Path::new(&format!("{log}.writer_release")), 8, false)?;
        }
        Ok(())
    })
}

fn persist_archive(scenario: &str, thread: &str) -> Result {
    let mut db = rusqlite::Connection::open(env("ARCHIVE_STATE"))?;
    let tx = db.transaction()?;
    tx.execute(
        "UPDATE threads SET archived=1, archived_at=60 WHERE id=?",
        [thread],
    )?;
    if matches!(
        scenario,
        "descendant_normal" | "descendant_gate" | "descendant_queued"
    ) {
        tx.execute(
            "UPDATE threads SET archived=1, archived_at=60 WHERE id='child'",
            [],
        )?;
    }
    tx.commit()?;
    Ok(())
}
