//! The production scheduler must wake without ingress or a delivery notification.
use crate::{
    app_backend::AppServerTurnBackend, queue_runner::QueueCoordinator, soak::native_fixture,
};
use cdr_app_server::ResidentAppServer;
use cdr_store::queue::{self, QueueJobState};
use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, watch};

const WRITER: &str = "app-server returned error -32600 for thread/resume: thread writer-fast already has an active writer";

async fn exercise(cold: bool, cancel: bool) {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("rpc.jsonl");
    let release = temp.path().join("released");
    let mut config = native_fixture::config("action");
    config.environment.insert(
        "CDR_ACTION_RPC_LOG".into(),
        log.to_string_lossy().into_owned(),
    );
    config.environment.insert(
        "CDR_WRITER_RELEASE_PATH".into(),
        release.to_string_lossy().into_owned(),
    );
    let server = Arc::new(ResidentAppServer::start(config).await.unwrap());
    let db = temp.path().join("queue.sqlite");
    let coordinator = Arc::new(QueueCoordinator::new(
        db.clone(),
        Arc::new(AppServerTurnBackend::new(Arc::clone(&server))),
    ));
    let accepted = coordinator
        .submit("writer-fast", 42, 1, Some(10), "original")
        .await
        .unwrap();
    assert!(accepted.warning.is_some());
    let coordinator = if cold {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        cdr_store::schema::open_initialized(&db).unwrap().execute(
            "UPDATE codex_turn_queue SET attempt_count=9,updated_at=?1,last_error=?2 WHERE job_id=?3",
            rusqlite::params![now-14.0,WRITER,accepted.job_id]).unwrap();
        Arc::new(QueueCoordinator::new(
            db.clone(),
            Arc::new(AppServerTurnBackend::new(Arc::clone(&server))),
        ))
    } else {
        coordinator
    };
    let client = Arc::new(
        twilight_http::Client::builder()
            .token("fixture".into())
            .build(),
    );
    let (_sender, receiver) = broadcast::channel(128);
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(super::super::run_completion_worker(
        receiver,
        Arc::clone(&server),
        Arc::clone(&coordinator),
        client,
        false,
        Duration::from_secs(2),
        shutdown,
    ));
    tokio::time::sleep(Duration::from_millis(250)).await;
    // No native notification or queue/delivery wake accompanies release.
    std::fs::write(&release, b"released").unwrap();
    if cancel {
        cdr_store::schema::open_initialized(&db)
            .unwrap()
            .execute(
                "DELETE FROM codex_turn_queue WHERE job_id=?1",
                [&accepted.job_id],
            )
            .unwrap();
    }
    let began = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if queue::list(&db)
                .unwrap()
                .iter()
                .any(|job| job.job_id == accepted.job_id && job.state == QueueJobState::Running)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(7), task)
        .await
        .unwrap()
        .unwrap();
    server.close().await.unwrap();
    let calls = std::fs::read_to_string(log).unwrap();
    let starts = calls
        .lines()
        .filter(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            value["method"] == "turn/start" && value["params"]["threadId"] == "writer-fast"
        })
        .count();
    if cancel {
        assert!(outcome.is_err());
        assert_eq!(starts, 0);
    } else {
        assert!(
            outcome.is_ok(),
            "writer release must not wait for the 30s global scan"
        );
        assert_eq!(starts, 1);
        assert!(began.elapsed() < Duration::from_secs(15));
    }
}

#[tokio::test]
async fn foreground_failure_wakes_production_scheduler_without_ingress() {
    exercise(false, false).await;
}

#[tokio::test]
async fn cold_count_nine_uses_original_updated_at_and_cancellation_cannot_start() {
    exercise(true, false).await;
    exercise(true, true).await;
}
