use super::*;
use crate::discord_dispatch::{
    BoxDiscordFuture, DispatchOutcome, InteractionDispatcher, InteractionTransport,
};
use cdr_discord::{
    gateway::ingress::InteractionIngressTag, interaction_access::InteractionAccessPolicy,
};
use twilight_model::{
    application::interaction::Interaction, http::interaction::InteractionResponse,
    id::marker::InteractionMarker,
};

async fn history(f: &MessageFixture, kind: &str) {
    f.server
        .execute(
            AppRequest {
                method: "test/history-kind",
                params: json!({"kind":kind}),
                timeout: Duration::from_secs(2),
            },
            None,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn choices_and_text_use_complete_lightweight_pages_even_if_full_history_hangs() {
    for text in [false, true] {
        let http = approval_http::start().await;
        let temp = tempfile::tempdir().unwrap();
        let f = fixture(&temp, http_client(&http.address)).await;
        let (id, work) = question(&f).await;
        queue::complete(f.executor.mirror_db(), "origin").unwrap();
        history(&f, "heavy-paged").await;
        let component = if text {
            ComponentId::AsyncTextAnswer {
                question_id: id.clone(),
                text: "다른 답변".into(),
            }
        } else {
            ComponentId::AsyncChoice {
                question_id: id.clone(),
                option: 1,
            }
        };
        handle_component_work(&work, &component, &f.executor, &f.server)
            .await
            .unwrap();
        let jobs = queue::list(f.executor.mirror_db()).unwrap();
        assert_eq!(jobs[0].baseline_turn_ids.len(), 201);
        assert!(jobs[0].baseline_turn_ids.iter().any(|v| v == "original"));
        assert_eq!(
            aq::get(f.executor.mirror_db(), &id).unwrap().state,
            "submitted"
        );
        f.server.close().await.unwrap();
        http.stop.send(()).unwrap();
        http.task.await.unwrap();
        let rpc = app_fixture::rpc_log(&temp.path().join("rpc.jsonl"));
        assert!(
            rpc.iter()
                .filter(|v| v["method"] == "thread/read")
                .all(|v| v["params"]["includeTurns"] == false)
        );
        let pages: Vec<_> = rpc
            .iter()
            .filter(|v| v["method"] == "thread/turns/list")
            .collect();
        assert_eq!(
            pages.len(),
            6,
            "all three pages must be rechecked before sending"
        );
        assert!(
            pages
                .iter()
                .all(|v| v["params"]["itemsView"] == "notLoaded")
        );
        assert_eq!(
            rpc.iter().filter(|v| v["method"] == "turn/start").count(),
            1
        );
    }
}

struct Ack;
impl InteractionTransport for Ack {
    fn acknowledge<'a>(
        &'a self,
        _: Id<InteractionMarker>,
        _: &'a str,
        _: &'a InteractionResponse,
    ) -> BoxDiscordFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn update<'a>(&'a self, _: &'a str, _: &'a str) -> BoxDiscordFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

async fn run_worker(f: MessageFixture, id: &str, work: &InboundInteractionWork) {
    let message = work.source_message_id.unwrap().to_string();
    let interaction:Interaction = serde_json::from_value(json!({
        "application_id":"1","authorizing_integration_owners":{},"channel_id":"42",
        "data":{"component_type":2,"custom_id":format!("codex_async:{id}:1")},
        "entitlements":[],"id":"501","locale":"en-US","token":"fixture","type":3,"version":1,
        "user":{"avatar":null,"bot":false,"discriminator":"0001","id":"3","username":"tester"},
        "message":{"id":message,"channel_id":"42","attachments":[],"embeds":[],"content":"question",
        "author":{"avatar":null,"bot":true,"discriminator":"0001","id":"1","username":"bot"},
        "mention_everyone":false,"mention_roles":[],"mentions":[],"pinned":false,"tts":false,"type":0,"timestamp":"2020-02-02T02:02:02.020000+00:00"}
    })).unwrap();
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let dispatcher = InteractionDispatcher::new(
        Arc::new(Ack),
        InteractionAccessPolicy {
            allow_all_channels: true,
            ..Default::default()
        },
        false,
        sender,
        f.executor.mirror_db(),
    );
    assert_eq!(
        dispatcher
            .dispatch(
                &interaction,
                tokio::time::Instant::now(),
                InteractionIngressTag::Normal
            )
            .await
            .unwrap(),
        DispatchOutcome::Queued
    );
    let row = cdr_store::ingress::get(f.executor.mirror_db(), "interaction:501")
        .unwrap()
        .unwrap();
    assert_eq!(row.target_thread_id.as_deref(), Some("thread-b"));
    drop(dispatcher);
    crate::interaction_worker::run_interaction_worker(
        receiver,
        Arc::new(f.executor),
        f.server,
        f.http,
    )
    .await;
}

async fn known_refusal(kind: &str, fail_notice: bool) {
    let http = approval_http::start().await;
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(&temp, http_client(&http.address)).await;
    let (id, work) = question(&f).await;
    let db = f.executor.mirror_db().to_owned();
    if kind != "pending" {
        queue::complete(&db, "origin").unwrap();
        history(&f, kind).await;
    }
    if fail_notice {
        cdr_store::schema::open_initialized(&db).unwrap().execute_batch(
            "CREATE TRIGGER fail_notice BEFORE UPDATE OF message_id ON codex_delivery_receipts BEGIN SELECT RAISE(ABORT,'notice confirmation failed'); END;"
        ).unwrap();
    }
    let server = f.server.clone();
    run_worker(f, &id, &work).await;
    let row = cdr_store::ingress::get(&db, "interaction:501")
        .unwrap()
        .unwrap();
    assert!(cdr_store::ingress::AsyncAnswerRefusal::from_outcome(row.outcome.as_ref()).is_some());
    assert_eq!(row.confirmation_delivered, !fail_notice);
    assert_eq!(row.state, "completed");
    assert_eq!(
        aq::get(&db, &id).unwrap().state,
        if kind == "second-read-timeout" {
            "rejected"
        } else {
            "open"
        }
    );
    if fail_notice {
        cdr_store::ingress::recover_prior_runtime(
            &db,
            "next-runtime",
            super::super::now().unwrap(),
        )
        .unwrap();
        let row = cdr_store::ingress::get(&db, "interaction:501")
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "held");
        assert!(row.hold_reason.contains("답변을 전송하지 않았습니다"));
        assert!(!row.hold_reason.contains("execution outcome is unknown"));
    }
    assert!(
        cdr_store::delivery::list_pending(&db)
            .unwrap()
            .iter()
            .all(|d| !d.content.contains("Saved Discord request"))
    );
    server.close().await.unwrap();
    http.stop.send(()).unwrap();
    let traffic = http.task.await.unwrap();
    let notices: Vec<_> = traffic
        .iter()
        .filter(|(_, b)| {
            b["content"]
                .as_str()
                .is_some_and(|v| v.starts_with("답변을 전송하지 않았습니다"))
        })
        .collect();
    assert_eq!(notices.len(), 1);
    assert!(
        !notices[0].1["content"]
            .as_str()
            .unwrap()
            .contains("SQLite integrity check failed")
    );
    let rpc = app_fixture::rpc_log(&temp.path().join("rpc.jsonl"));
    assert!(
        !rpc.iter()
            .any(|v| matches!(v["method"].as_str(), Some("turn/start" | "turn/steer")))
    );
}

#[tokio::test]
async fn pending_work_gets_one_truthful_notice_and_keeps_answer_open() {
    Box::pin(known_refusal("pending", false)).await;
}

#[tokio::test]
async fn read_timeout_before_claim_is_not_an_unknown_execution() {
    Box::pin(known_refusal("metadata-timeout", false)).await;
}

#[tokio::test]
async fn read_timeout_after_claim_is_still_proven_no_send() {
    Box::pin(known_refusal("second-read-timeout", false)).await;
}

#[tokio::test]
async fn lost_refusal_notice_keeps_no_send_evidence_after_restart() {
    Box::pin(known_refusal("pending", true)).await;
}

#[tokio::test]
async fn timeout_after_actual_start_keeps_unknown_outcome_without_replay() {
    let http = approval_http::start().await;
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(&temp, http_client(&http.address)).await;
    let (id, work) = question(&f).await;
    let db = f.executor.mirror_db().to_owned();
    queue::complete(&db, "origin").unwrap();
    control(&f, "test/drop-reply").await;
    let server = f.server.clone();
    run_worker(f, &id, &work).await;
    let row = cdr_store::ingress::get(&db, "interaction:501")
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "held");
    assert!(row.outcome.is_none());
    assert_eq!(row.target_thread_id.as_deref(), Some("thread-b"));
    assert_eq!(aq::get(&db, &id).unwrap().state, "dispatching");
    assert!(
        cdr_store::delivery::list_pending(&db)
            .unwrap()
            .iter()
            .any(|d| d.content.contains("execution outcome is unknown"))
    );
    server.close().await.unwrap();
    http.stop.send(()).unwrap();
    http.task.await.unwrap();
    let rpc = app_fixture::rpc_log(&temp.path().join("rpc.jsonl"));
    assert_eq!(
        rpc.iter().filter(|v| v["method"] == "turn/start").count(),
        1
    );
}
