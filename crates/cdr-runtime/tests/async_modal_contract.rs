use cdr_discord::{
    components::ComponentId, gateway::ingress::InteractionIngressTag, interaction::RoutedWork,
};
use cdr_runtime::discord_dispatch::{DispatchOutcome, InteractionDispatcher};
use cdr_store::{async_question as aq, delivery_receipt, queue};
use serde_json::json;
use std::sync::Arc;
use tokio::{sync::mpsc, time::Instant};
use twilight_model::{
    application::interaction::Interaction, http::interaction::InteractionResponseType,
};

#[path = "support/interaction_dispatch.rs"]
mod database;
#[path = "support/interaction_custody.rs"]
mod support;

fn question(db: &std::path::Path) -> String {
    let conn = cdr_store::schema::open_initialized(db).unwrap();
    conn.execute(
        "INSERT INTO mirror_threads VALUES ('thread','project','title',1,10,0)",
        [],
    )
    .unwrap();
    queue::enqueue(
        db,
        queue::NewQueueJob {
            job_id: "origin",
            target_thread_id: "thread",
            channel_id: 10,
            owner_user_id: Some(20),
            discord_message_id: Some(30),
            app_server_generation: 1,
            prompt: "original",
            queued: false,
            ack_sent: true,
            created_at: 1.0,
        },
    )
    .unwrap();
    queue::begin_attempt(db, "origin", &[], 1).unwrap();
    queue::mark_running(db, "origin", "original-turn", 1).unwrap();
    let id = aq::observe(
        db,
        &aq::NewQuestion {
            runtime_id: "fixture",
            generation: 1,
            thread_id: "thread",
            turn_id: "original-turn",
            item_id: "question-item",
            body: &aq::QuestionBody {
                index: 0,
                source_text: String::new(),
                title: "Which?".into(),
                options: vec!["A".into(), "B".into()],
            },
            now: support::now(),
        },
    )
    .unwrap();
    let key = aq::receipt_key(&aq::get(db, &id).unwrap()).unwrap();
    delivery_receipt::begin(db, &key, "hash").unwrap();
    delivery_receipt::confirm(db, &key, "1234").unwrap();
    aq::bind_receipt(db, &id, true).unwrap();
    id
}

fn interaction(sequence: u64, id: &str, answer: Option<&str>, message: u64) -> Interaction {
    let mut value = serde_json::to_value(support::busy_component(
        sequence, "secret", "unused", "queue",
    ))
    .unwrap();
    value["message"] = json!({"id":message.to_string(),"channel_id":"10","attachments":[],
        "author":{"avatar":null,"bot":true,"discriminator":"0001","id":"2","username":"fixture"},
        "content":"question","embeds":[],"mention_everyone":false,"mention_roles":[],"mentions":[],
        "pinned":false,"timestamp":"2020-02-02T02:02:02.020000+00:00","tts":false,"type":0});
    if let Some(text) = answer {
        value["type"] = json!(5);
        value["data"] = json!({"custom_id":format!("codex_async_reply:{id}"),"components":[{
            "type":18,"id":1,"component":{"type":4,"id":2,"custom_id":"answer","value":text}
        }]});
    } else {
        value["data"] = json!({"component_type":2,"custom_id":format!("codex_async_other:{id}")});
    }
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn opening_or_cancelling_input_does_not_claim_question_and_submit_routes_exact_text() {
    let db = database::DispatchDatabase::new();
    let id = question(db.path());
    let transport = Arc::new(support::InspectingTransport::new(db.path()));
    let (sender, mut receiver) = mpsc::channel(2);
    let dispatcher = InteractionDispatcher::new(
        transport.clone(),
        support::policy(),
        false,
        sender,
        db.path(),
    );
    for sequence in 100..102 {
        assert_eq!(
            dispatcher
                .dispatch(
                    &interaction(sequence, &id, None, 1234),
                    Instant::now(),
                    InteractionIngressTag::Normal
                )
                .await
                .unwrap(),
            DispatchOutcome::RespondedWithoutWork
        );
        assert_eq!(
            transport.responses.lock().unwrap().last().unwrap().kind,
            InteractionResponseType::Modal
        );
        assert!(receiver.try_recv().is_err());
        assert!(
            cdr_store::ingress::by_origin(db.path(), i64::try_from(sequence).unwrap())
                .unwrap()
                .is_none()
        );
        assert_eq!(aq::get(db.path(), &id).unwrap().state, "open");
    }
    // A copied button on another source message must not open the input window.
    dispatcher
        .dispatch(
            &interaction(102, &id, None, 9999),
            Instant::now(),
            InteractionIngressTag::Normal,
        )
        .await
        .unwrap();
    assert_ne!(
        transport.responses.lock().unwrap().last().unwrap().kind,
        InteractionResponseType::Modal
    );
    assert!(receiver.try_recv().is_err());
    let text = "별도 답변\nA=B | C=보류";
    assert_eq!(
        dispatcher
            .dispatch(
                &interaction(103, &id, Some(text), 1234),
                Instant::now(),
                InteractionIngressTag::Normal
            )
            .await
            .unwrap(),
        DispatchOutcome::Queued
    );
    let work = receiver.recv().await.unwrap();
    assert_eq!(work.source_message_id.unwrap().get(), 1234);
    assert_eq!(
        work.work,
        RoutedWork::Component(ComponentId::AsyncTextAnswer {
            question_id: id.clone(),
            text: text.into()
        })
    );
    assert_eq!(
        transport.responses.lock().unwrap().last().unwrap().kind,
        InteractionResponseType::DeferredUpdateMessage
    );
    assert_eq!(
        aq::get(db.path(), &id).unwrap().state,
        "open",
        "only the execution worker may acquire answer custody"
    );
}
