//! Actual common consumers and fatal-exit notifier; all external I/O is fake.
use super::*;
use cdr_discord::gateway::ingress::{GatewayIngress, GatewayIngressConfig};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use twilight_model::gateway::{
    event::Event,
    payload::incoming::{InteractionCreate, MessageCreate, Ready},
};

#[allow(
    clippy::too_many_lines,
    reason = "one actual shared-consumer scenario preserves setup, failures, and fatal-notifier assertions together"
)]
pub(crate) async fn assert_common_consumers_survive(
    routing: crate::agy::routing::AgyRouting,
    policy: Arc<crate::config::RuntimeConfig>,
    mut failures: mpsc::Receiver<String>,
    agy_room: u64,
) {
    let temp = tempfile::tempdir().unwrap();
    let fixture = crate::test_support::message_fixture::MessageFixture::new(
        &temp,
        Arc::new(twilight_http::Client::new("fixture".into())),
    )
    .await;
    let mut config = policy.as_ref().clone();
    config.history_poll_interval = None;
    let admission = crate::restart_readiness::drain::AdmissionGate::new();
    let (work, _) = mpsc::channel(1);
    let resources = InteractionResources::new(
        Arc::new(cdr_discord::interaction_access::InteractionAccessPolicy::default()),
        work,
        Arc::new(crate::discord_dispatch::AutocompleteCatalog::default()),
        admission.clone(),
        fixture.executor.mirror_db().into(),
    );
    let server = Arc::clone(&fixture.server);
    let context = TypedIngressContext::new(
        Arc::new(config),
        fixture.http,
        Arc::new(fixture.executor),
        Arc::clone(&server),
        temp.path().into(),
        Arc::new(reqwest::Client::new()),
        resources,
        admission,
    )
    .with_agy(Some(routing));
    let (gateway, lanes) = GatewayIngress::new(GatewayIngressConfig {
        message_capacity: 1,
        ..Default::default()
    })
    .unwrap();
    let gaps = gateway.subscribe_message_gaps();
    let ready:Ready=serde_json::from_value(serde_json::json!({"v":10,"application":{"id":"2","flags":0},"guilds":[],"resume_gateway_url":"wss://gateway.invalid","session_id":"fixture","user":{"id":"9","username":"fixture","discriminator":"0001","avatar":null,"bot":true,"mfa_enabled":false}})).unwrap();
    let _ = gateway.publish(Event::Ready(ready), tokio::time::Instant::now());
    for id in [1, 2] {
        let message:MessageCreate=serde_json::from_value(serde_json::json!({"attachments":[],"author":{"avatar":null,"bot":false,"discriminator":"0001","id":"7","username":"fixture"},"channel_id":agy_room.to_string(),"guild_id":"1","content":"!status","edited_timestamp":null,"embeds":[],"id":id.to_string(),"mention_everyone":false,"mention_roles":[],"mentions":[],"pinned":false,"timestamp":"2026-10-28T00:00:00+00:00","tts":false,"type":0})).unwrap();
        let _ = gateway.publish(
            Event::MessageCreate(Box::new(message)),
            tokio::time::Instant::now(),
        );
    }
    for kind in [2, 3, 4] {
        let mut event:InteractionCreate=serde_json::from_value(serde_json::json!({"application_id":"2","authorizing_integration_owners":{},"id":(100+kind).to_string(),"token":"fixture","type":2,"guild_id":"1","channel_id":"999","user":{"id":"7","username":"fixture","discriminator":"0001","avatar":null},"data":{"id":"1","name":"status","type":1}})).unwrap();
        if kind == 3 {
            event.0.kind =
                twilight_model::application::interaction::InteractionType::MessageComponent;
            event.0.data = None;
        }
        if kind == 4 {
            event.0.kind=twilight_model::application::interaction::InteractionType::ApplicationCommandAutocomplete;
        }
        let _ = gateway.publish(
            Event::InteractionCreate(Box::new(event)),
            tokio::time::Instant::now(),
        );
    }
    let (stop, rx) = watch::channel(false);
    let (notifier, mut fatal) = super::super::worker_supervision::exit_channel();
    let mut history_worker = spawn_monitored(
        "common-history",
        notifier.clone(),
        history::run(
            gateway.subscribe_identity(),
            gateway.subscribe_identity_conflict(),
            gaps.resubscribe(),
            context.clone(),
            rx.clone(),
        ),
    );
    let mut interactions = spawn_monitored(
        "common-interactions",
        notifier,
        interaction::run_normal(
            lanes.normal_interactions,
            interaction::DiscordInteractionHandler::from_context(&context),
            rx,
        ),
    );
    for _ in 0..4 {
        tokio::time::timeout(Duration::from_secs(2), failures.recv())
            .await
            .unwrap()
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        fatal.try_recv().is_err(),
        "AGY error/panic escaped into common fatal notifier"
    );
    assert_eq!(
        gaps.snapshot().unwrap().len(),
        1,
        "failed history must keep its gap"
    );
    stop.send(true).unwrap();
    history_worker.join().await.unwrap().unwrap();
    interactions.join().await.unwrap().unwrap();
    server.close().await.unwrap();
}
