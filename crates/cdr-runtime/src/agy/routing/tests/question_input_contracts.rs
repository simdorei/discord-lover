use super::*;
use cdr_discord::{
    components::ComponentId,
    interaction::{RoutedWork, route_component, route_modal},
};
use twilight_model::{
    application::interaction::InteractionData, http::interaction::InteractionResponseType,
};

const ANSWER: &str = "  다른 답변\nA=B | C=보류  ";

fn question_input(id: u64, channel: u64, submit: bool) -> InteractionCreate {
    let mut value = serde_json::to_value(interaction(id)).unwrap();
    let question = "a".repeat(64);
    value["channel_id"] = serde_json::json!(channel.to_string());
    value["message"] = serde_json::to_value(message(1234, channel, "question")).unwrap();
    if submit {
        value["type"] = serde_json::json!(5);
        value["data"] = serde_json::json!({
            "custom_id": format!("codex_async_reply:{question}"),
            "components": [{"type":18,"id":1,"component":{
                "type":4,"id":2,"custom_id":"answer","value":ANSWER
            }}]
        });
    } else {
        value["type"] = serde_json::json!(3);
        value["data"] = serde_json::json!({
            "component_type":2,"custom_id":format!("codex_async_other:{question}")
        });
    }
    serde_json::from_value(value).unwrap()
}

fn assert_codex_input(event: &InteractionCreate, submit: bool) {
    let routed = match event.data.as_ref().unwrap() {
        InteractionData::MessageComponent(data) => route_component(data),
        InteractionData::ModalSubmit(data) => route_modal(data),
        other => panic!("unexpected question input: {other:?}"),
    }
    .unwrap();
    let question_id = "a".repeat(64);
    assert_eq!(
        routed.work,
        Some(RoutedWork::Component(if submit {
            ComponentId::AsyncTextAnswer {
                question_id,
                text: ANSWER.into(),
            }
        } else {
            ComponentId::AsyncTextInput { question_id }
        }))
    );
    assert_eq!(
        routed.initial_response.kind,
        if submit {
            InteractionResponseType::DeferredUpdateMessage
        } else {
            InteractionResponseType::Modal
        }
    );
}

#[tokio::test]
async fn codex_question_inputs_preserve_bindings_and_never_execute_in_agy_rooms() {
    for phase in [Phase::Ready, Phase::Off, Phase::Faulted] {
        let mut f = fixture().await;
        let service = (phase == Phase::Ready).then(|| Arc::clone(&f.service));
        let (manager, _status, _failures) =
            routing_handle(service, Arc::clone(&f.service.ownership), phase);
        let acks = Arc::new(FakeInteractions::default());
        let (stop, rx) = watch::channel(false);
        let lanes = AgyRouting::start_with_adapters(
            manager,
            Arc::clone(&f.service.policy),
            acks.clone(),
            rx,
            Arc::clone(&f.service.remote),
            f.messages.clone(),
        );
        let (gateway, mut receivers) =
            GatewayIngress::new(GatewayIngressConfig::default()).unwrap();
        let (resume, mut resumed) = mpsc::channel(4);
        let reads_before = f.remote.reads.load(Ordering::SeqCst);

        for submit in [false, true] {
            let event = question_input(1000 + u64::from(submit), 90, submit);
            let original = serde_json::to_value(&event).unwrap();
            let at = Instant::now();
            let _ = gateway.publish(Event::InteractionCreate(Box::new(event)), at);
            let routed = lanes
                .routing
                .interaction(
                    receivers.normal_interactions.recv().await.unwrap(),
                    resume.clone(),
                )
                .expect("known Codex input must pass through every AGY phase");
            assert_eq!(routed.received_at, at);
            assert_eq!(serde_json::to_value(&routed.event).unwrap(), original);
            assert_codex_input(&routed.event, submit);
        }
        assert!(acks.acknowledgments.lock().unwrap().is_empty());

        for submit in [false, true] {
            let _ = gateway.publish(
                Event::InteractionCreate(Box::new(question_input(
                    2000 + u64::from(submit),
                    f.thread,
                    submit,
                ))),
                Instant::now(),
            );
            assert!(
                lanes
                    .routing
                    .interaction(
                        receivers.normal_interactions.recv().await.unwrap(),
                        resume.clone(),
                    )
                    .is_none(),
                "a copied Codex input in an AGY room must not reach Codex"
            );
        }
        tokio::time::timeout(Duration::from_millis(2500), async {
            while acks.acknowledgments.lock().unwrap().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let observed = acks.acknowledgments.lock().unwrap().clone();
        assert_eq!(observed.len(), 2);
        assert!(
            observed
                .iter()
                .all(|(_, _, kind)| { *kind == InteractionResponseType::ChannelMessageWithSource })
        );
        assert_eq!(acks.updates.load(Ordering::SeqCst), 0);
        assert!(resumed.try_recv().is_err());
        assert!(f.receiver.try_recv().is_err());
        assert_eq!(f.remote.reads.load(Ordering::SeqCst), reads_before);
        let db =
            rusqlite::Connection::open(f.temp.path().join(".codex_discord_agy.sqlite")).unwrap();
        let requests: i64 = db
            .query_row("SELECT count(*) FROM requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(requests, 0);
        assert!(f.service.quiescent());
        stop.send(true).unwrap();
        lanes
            .join(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }
}
