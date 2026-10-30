use cdr_app_server::ResidentAppServer;
use cdr_runtime::{
    app_backend::AppServerTurnBackend, queue_runner::TurnBackend, soak::native_fixture,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

async fn fixture(
    script: Value,
) -> (
    tempfile::TempDir,
    Arc<ResidentAppServer>,
    AppServerTurnBackend,
) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("script.json");
    std::fs::write(&path, script.to_string()).unwrap();
    let mut config = native_fixture::config("async-history");
    config.environment.insert(
        "CDR_ASYNC_HISTORY_SCRIPT".into(),
        path.to_string_lossy().into_owned(),
    );
    config.environment.insert(
        "CDR_ASYNC_HISTORY_LOG".into(),
        temp.path().join("rpc.jsonl").to_string_lossy().into_owned(),
    );
    let server = Arc::new(ResidentAppServer::start(config).await.unwrap());
    let backend = AppServerTurnBackend::new(server.clone())
        .with_timeouts(Duration::from_secs(2), Duration::from_millis(500));
    (temp, server, backend)
}

#[tokio::test]
async fn baseline_reads_all_pages_without_loading_conversation_items() {
    let (temp, server, backend) = fixture(json!({"pages":[
        {"data":[{"id":"newer","status":"inProgress","items":[],"itemsStatus":"notLoaded"}],"nextCursor":"page-1"},
        {"data":[{"id":"older","status":"completed","items":[],"itemsStatus":"notLoaded"}],"nextCursor":null}
    ]})).await;
    let turns = backend.read_turns("thread").await.unwrap();
    assert_eq!(turns.len(), 2);
    assert!(turns.iter().any(|t| t.turn_id == "older"));
    assert!(turns.iter().any(|t| t.turn_id == "newer"));
    server.close().await.unwrap();
    let rpc: Vec<Value> = std::fs::read_to_string(temp.path().join("rpc.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let reads: Vec<_> = rpc
        .iter()
        .filter(|r| r["method"] == "thread/read")
        .collect();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0]["params"]["includeTurns"], false);
    let pages: Vec<_> = rpc
        .iter()
        .filter(|r| r["method"] == "thread/turns/list")
        .collect();
    assert_eq!(pages.len(), 2);
    assert!(
        pages
            .iter()
            .all(|r| r["params"]["itemsView"] == "notLoaded")
    );
    assert_eq!(pages[1]["params"]["cursor"], "page-1");
    assert!(
        !rpc.iter()
            .any(|r| matches!(r["method"].as_str(), Some("turn/start" | "thread/resume")))
    );
}

#[tokio::test]
async fn incomplete_or_invalid_turn_index_is_never_a_start_baseline() {
    let turn = json!({"id":"one","status":"completed"});
    for script in [
        json!({"wrong_thread":"other","pages":[]}),
        json!({"pages":[{"data":[turn.clone()],"truncated":true}]}),
        json!({"pages":[{"data":[{"id":"one","status":"unknown"}],"nextCursor":null}]}),
        json!({"pages":[{"data":[turn.clone(),turn.clone()],"nextCursor":null}]}),
        json!({"pages":[{"data":[turn.clone()],"nextCursor":"page-1"},{"data":[turn.clone()],"nextCursor":null}]}),
        json!({"pages":[{"data":[turn.clone()],"nextCursor":"page-1"},{"data":[{"id":"two","status":"completed"}],"nextCursor":"page-1"}]}),
        json!({"pages":[{"data":[],"nextCursor":"page-1"}]}),
        json!({"pages":[{"data":[turn.clone()],"nextCursor":42}]}),
        json!({"pages":[{"data":vec![turn;101],"nextCursor":null}]}),
        json!({"timeout":true}),
    ] {
        let (_temp, server, backend) = fixture(script.clone()).await;
        let result = backend.read_turns("thread").await;
        assert!(result.is_err(), "accepted invalid baseline: {script}");
        assert!(!result.unwrap_err().ambiguous, "no mutation was dispatched");
        server.close().await.unwrap();
    }
}
