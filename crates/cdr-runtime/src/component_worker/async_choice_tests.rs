use super::*;
use crate::{
    soak::native_fixture,
    test_support::{app_fixture, approval_http, message_fixture::MessageFixture},
};
use cdr_app_server::{ResidentNotificationEvent, requests::AppRequest};
use cdr_store::{async_question as aq, queue};
use serde_json::json;
use std::time::Duration;
use twilight_model::id::Id;

async fn fixture(temp: &tempfile::TempDir, http: Arc<Client>) -> MessageFixture {
    MessageFixture::with_server(temp, http, start_server(temp).await)
}

async fn start_server(temp: &tempfile::TempDir) -> Arc<ResidentAppServer> {
    let mut config = native_fixture::config("async-question");
    config.environment.insert(
        "CDR_ACTION_RPC_LOG".into(),
        temp.path().join("rpc.jsonl").to_string_lossy().into_owned(),
    );
    Arc::new(ResidentAppServer::start(config).await.unwrap())
}

async fn control(f: &MessageFixture, method: &'static str) {
    f.server
        .execute(
            AppRequest {
                method,
                params: json!({}),
                timeout: Duration::from_secs(2),
            },
            None,
        )
        .await
        .unwrap();
}

async fn question(f: &MessageFixture) -> (String, InboundInteractionWork) {
    let db = f.executor.mirror_db();
    queue::enqueue(
        db,
        queue::NewQueueJob {
            job_id: "origin",
            target_thread_id: "thread-b",
            channel_id: 42,
            owner_user_id: Some(3),
            discord_message_id: Some(40),
            app_server_generation: 1,
            prompt: "original prompt",
            queued: false,
            ack_sent: true,
            created_at: 1.0,
        },
    )
    .unwrap();
    queue::begin_attempt(db, "origin", &[], 1).unwrap();
    queue::mark_running(db, "origin", "original", 1).unwrap();
    let mut notifications = f.server.subscribe_notifications();
    control(f, "test/question").await;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), notifications.recv())
            .await
            .unwrap()
            .unwrap();
        if let ResidentNotificationEvent::Notification {
            generation,
            notification,
        } = event
            && notification.method == "item/completed"
        {
            crate::async_question_ui::observe(
                db,
                f.server.instance_id(),
                generation,
                &notification.params,
            )
            .unwrap();
            break;
        }
    }
    crate::async_question_ui::deliver_pending(db, f.server.instance_id(), 1, &f.http)
        .await
        .unwrap();
    let id = aq::occurrence_id("thread-b", "original", "question-call", 1).unwrap();
    let q = aq::get(db, &id).unwrap();
    let component = ComponentId::AsyncChoice {
        question_id: id.clone(),
        option: 1,
    };
    let work = InboundInteractionWork {
        application_id: Id::new(1),
        interaction_id: Id::new(501),
        channel_id: Id::new(42),
        user_id: Id::new(3),
        source_message_id: Some(Id::new(q.message_id.unwrap().parse().unwrap())),
        interaction_token: "fixture".into(),
        work: cdr_discord::interaction::RoutedWork::Component(component),
        processing_mode: InteractionProcessingMode::Execute,
        custody_database: db.to_owned(),
        custody_ingress_id: "fixture-click".into(),
        authorized_busy_choice: None,
        admission_permit: None,
    };
    (id, work)
}

fn http_client(address: &str) -> Arc<Client> {
    Arc::new(
        Client::builder()
            .token("fixture-token".into())
            .proxy(address.into(), true)
            .ratelimiter(None)
            .build(),
    )
}

#[tokio::test]
async fn async_choice_wire_metadata_buttons_and_exact_original_steer_survive_duplicate_click() {
    let http = approval_http::start().await;
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(&temp, http_client(&http.address)).await;
    control(&f, "test/active-turn").await;
    let (id, work) = question(&f).await;
    let component = ComponentId::AsyncChoice {
        question_id: id.clone(),
        option: 1,
    };
    let (first, second) = tokio::join!(
        handle_component_work(&work, &component, &f.executor, &f.server),
        handle_component_work(&work, &component, &f.executor, &f.server)
    );
    assert!(first.is_ok() && second.is_ok());
    assert_eq!(
        aq::get(f.executor.mirror_db(), &id).unwrap().state,
        "submitted"
    );
    let other = aq::occurrence_id("thread-b", "original", "question-call", 0).unwrap();
    assert_eq!(
        aq::get(f.executor.mirror_db(), &other).unwrap().state,
        "open"
    );
    // Replayed producer item must not POST any question twice.
    crate::async_question_ui::deliver_pending(
        f.executor.mirror_db(),
        f.server.instance_id(),
        1,
        &f.http,
    )
    .await
    .unwrap();
    f.server.close().await.unwrap();
    http.stop.send(()).unwrap();
    let traffic = http.task.await.unwrap();
    assert_eq!(
        traffic.len(),
        5,
        "one original context, two distinct question bodies and two controls messages"
    );
    let controls: Vec<_> = traffic
        .iter()
        .filter(|(_, body)| body["components"].as_array().is_some_and(|r| !r.is_empty()))
        .collect();
    assert_eq!(controls.len(), 2);
    assert_eq!(
        controls[1].1["components"][0]["components"][1]["custom_id"],
        format!("codex_async:{id}:1")
    );
    let rpc = app_fixture::rpc_log(&temp.path().join("rpc.jsonl"));
    let steers: Vec<_> = rpc.iter().filter(|v| v["method"] == "turn/steer").collect();
    assert_eq!(steers.len(), 1);
    assert_eq!(steers[0]["params"]["expectedTurnId"], "original");
    let prompt = steers[0]["params"]["input"][0]["text"].as_str().unwrap();
    let answer: serde_json::Value =
        serde_json::from_str(prompt.split_once('\n').unwrap().1).unwrap();
    assert_eq!(answer["question_index"], 1);
    assert_eq!(answer["question_title"], "프로젝트 B?");
    assert_eq!(answer["selected_option"], "보류");
    assert!(!rpc.iter().any(|v| matches!(
        v["method"].as_str(),
        Some("thread/fork" | "thread/resume" | "turn/start")
    )));
}

#[tokio::test]
async fn async_choice_completed_start_rechecks_successor_at_actual_send_boundary() {
    for stale in [false, true] {
        let http = approval_http::start().await;
        let temp = tempfile::tempdir().unwrap();
        let f = fixture(&temp, http_client(&http.address)).await;
        let (id, work) = question(&f).await;
        queue::complete(f.executor.mirror_db(), "origin").unwrap();
        if stale {
            control(&f, "test/stale-on-second-read").await;
        }
        let result = handle_component_work(
            &work,
            &ComponentId::AsyncChoice {
                question_id: id.clone(),
                option: 1,
            },
            &f.executor,
            &f.server,
        )
        .await;
        assert_eq!(
            result.is_ok(),
            !stale,
            "{:?}",
            result.as_ref().err().map(ToString::to_string)
        );
        let q = aq::get(f.executor.mirror_db(), &id).unwrap();
        assert_eq!(q.state, if stale { "rejected" } else { "submitted" });
        let jobs = queue::list(f.executor.mirror_db()).unwrap();
        assert_eq!(jobs.len(), usize::from(!stale));
        if !stale {
            assert_eq!(jobs[0].turn_id.as_deref(), Some("answer-turn"));
        }
        f.server.close().await.unwrap();
        http.stop.send(()).unwrap();
        http.task.await.unwrap();
        let rpc = app_fixture::rpc_log(&temp.path().join("rpc.jsonl"));
        assert_eq!(
            rpc.iter().filter(|v| v["method"] == "turn/start").count(),
            usize::from(!stale)
        );
        assert!(!rpc.iter().any(|v| v["method"] == "turn/steer"));
    }
}

#[tokio::test]
async fn async_choice_lost_start_response_survives_real_queue_recovery_without_adoption_or_retry() {
    let http = approval_http::start().await;
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(&temp, http_client(&http.address)).await;
    let (id, work) = question(&f).await;
    queue::complete(f.executor.mirror_db(), "origin").unwrap();
    control(&f, "test/drop-reply").await;
    let result = handle_component_work(
        &work,
        &ComponentId::AsyncChoice {
            question_id: id.clone(),
            option: 1,
        },
        &f.executor,
        &f.server,
    )
    .await;
    assert!(result.is_err());
    assert_eq!(
        aq::get(f.executor.mirror_db(), &id).unwrap().state,
        "dispatching"
    );
    assert_eq!(
        queue::list(f.executor.mirror_db()).unwrap()[0].state,
        queue::QueueJobState::Quarantined
    );
    f.queue.recover_target("thread-b").await.unwrap();
    f.server.close().await.unwrap();
    // New owner/generation cannot make unknown input selectable or recoverable.
    let restarted = start_server(&temp).await;
    let recovered = crate::queue_runner::QueueCoordinator::new(
        f.executor.mirror_db().into(),
        Arc::new(crate::app_backend::AppServerTurnBackend::new(
            restarted.clone(),
        )),
    );
    recovered.recover_target("thread-b").await.unwrap();
    // Even an old generic Starting encoding, expired lease and changed generation
    // cannot enter empty-history retry or unrelated single-candidate adoption.
    cdr_store::schema::open_initialized(f.executor.mirror_db()).unwrap().execute_batch(
        "UPDATE codex_turn_queue SET state='starting',turn_id=NULL,app_server_generation=0,updated_at=0,last_error='lost response',baseline_turn_ids='[]';"
    ).unwrap();
    recovered.recover_target("thread-b").await.unwrap();
    let still_held = queue::list(f.executor.mirror_db()).unwrap();
    assert_eq!(still_held[0].state, queue::QueueJobState::Starting);
    assert_eq!(still_held[0].app_server_generation, 0);
    assert!(
        handle_component_work(
            &work,
            &ComponentId::AsyncChoice {
                question_id: id.clone(),
                option: 1
            },
            &f.executor,
            &restarted
        )
        .await
        .is_err()
    );
    assert_eq!(
        aq::get(f.executor.mirror_db(), &id).unwrap().state,
        "dispatching"
    );
    restarted.close().await.unwrap();
    http.stop.send(()).unwrap();
    http.task.await.unwrap();
    let rpc = app_fixture::rpc_log(&temp.path().join("rpc.jsonl"));
    assert_eq!(
        rpc.iter().filter(|v| v["method"] == "turn/start").count(),
        1
    );
    assert!(
        !rpc.iter()
            .any(|v| matches!(v["method"].as_str(), Some("thread/resume" | "thread/fork")))
    );
    let start = rpc
        .iter()
        .position(|v| v["method"] == "turn/start")
        .unwrap();
    assert_eq!(
        rpc[..start]
            .iter()
            .filter(|v| v["method"] == "thread/read")
            .count(),
        2
    );
    assert!(
        !rpc[start + 1..]
            .iter()
            .any(|v| v["method"] == "thread/read")
    );
}

#[path = "async_baseline_tests.rs"]
mod baseline;

#[path = "async_integration_tests.rs"]
mod integrated;

#[path = "async_inherited_tests.rs"]
mod inherited;

#[path = "async_answer_read_tests.rs"]
mod read_path;

#[tokio::test]
async fn free_text_reaches_only_original_question_once_without_parsing_as_choices() {
    let http = approval_http::start().await;
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(&temp, http_client(&http.address)).await;
    control(&f, "test/active-turn").await;
    let (id, mut work) = question(&f).await;
    let text = "제 선택은 따로 있어요\nA=B | C=보류 🙂";
    let component = ComponentId::AsyncTextAnswer {
        question_id: id.clone(),
        text: text.into(),
    };
    work.work = cdr_discord::interaction::RoutedWork::Component(component.clone());
    let source = work.source_message_id;
    work.source_message_id = Some(Id::new(999));
    assert!(
        handle_component_work(&work, &component, &f.executor, &f.server)
            .await
            .is_err()
    );
    assert_eq!(aq::get(f.executor.mirror_db(), &id).unwrap().state, "open");
    work.source_message_id = source;
    let (a, b) = tokio::join!(
        handle_component_work(&work, &component, &f.executor, &f.server),
        handle_component_work(&work, &component, &f.executor, &f.server)
    );
    assert!(a.is_ok() && b.is_ok());
    let q = aq::get(f.executor.mirror_db(), &id).unwrap();
    assert_eq!(q.state, "submitted");
    assert_eq!(q.chosen, None);
    assert_eq!(q.chosen_text.as_deref(), Some(text));
    let other = aq::occurrence_id("thread-b", "original", "question-call", 0).unwrap();
    assert_eq!(
        aq::get(f.executor.mirror_db(), &other).unwrap().state,
        "open"
    );
    assert!(matches!(
        handle_component_work(
            &work,
            &ComponentId::AsyncChoice {
                question_id: id,
                option: 0
            },
            &f.executor,
            &f.server
        )
        .await,
        Err(ComponentWorkerError::AlreadyHandled)
    ));
    f.server.close().await.unwrap();
    http.stop.send(()).unwrap();
    http.task.await.unwrap();
    let rpc = app_fixture::rpc_log(&temp.path().join("rpc.jsonl"));
    let steers: Vec<_> = rpc.iter().filter(|r| r["method"] == "turn/steer").collect();
    assert_eq!(steers.len(), 1);
    assert_eq!(steers[0]["params"]["expectedTurnId"], "original");
    let prompt = steers[0]["params"]["input"][0]["text"].as_str().unwrap();
    let data: serde_json::Value = serde_json::from_str(prompt.split_once('\n').unwrap().1).unwrap();
    assert_eq!(data["question_index"], 1);
    assert_eq!(data["question_title"], "프로젝트 B?");
    assert_eq!(data["answer_text"], text);
    assert!(data.get("selected_option").is_none());
    assert!(!rpc.iter().any(|r| r["method"] == "turn/start"));
}
