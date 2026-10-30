use std::sync::Arc;

use cdr_discord::gateway::ingress::InteractionIngressTag;
use cdr_runtime::discord_dispatch::{DispatchOutcome, InteractionDispatcher};
use cdr_store::claims::{NewBusyChoice, create_busy_choice};
use tokio::{sync::mpsc, time::Instant};

#[path = "support/interaction_dispatch.rs"]
mod dispatch_support;
use dispatch_support::DispatchDatabase;
#[path = "support/interaction_custody.rs"]
mod custody_support;
use custody_support::{InspectingTransport, busy_component, command, now, policy};

#[tokio::test]
async fn executable_interaction_is_token_free_and_staged_before_ack() {
    let database = DispatchDatabase::new();
    let transport = Arc::new(InspectingTransport::new(database.path()));
    let (sender, mut receiver) = mpsc::channel(1);
    let dispatcher = InteractionDispatcher::new(
        Arc::clone(&transport),
        policy(),
        false,
        sender,
        database.path(),
    );
    let secret = "unique-discord-interaction-token";

    assert_eq!(
        dispatcher
            .dispatch(
                &command(901, secret),
                Instant::now(),
                InteractionIngressTag::Normal,
            )
            .await
            .unwrap(),
        DispatchOutcome::Queued
    );
    {
        let seen = transport.seen_at_ack.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].state, "staged");
        assert!(!seen[0].payload.to_string().contains(secret));
    }
    let stored = cdr_store::ingress::by_origin(database.path(), 901)
        .unwrap()
        .unwrap();
    assert_eq!(stored.state, "acknowledged");
    assert!(!stored.payload.to_string().contains(secret));
    let database_bytes = std::fs::read(database.path()).unwrap();
    assert!(
        !database_bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes()),
        "Discord response tokens must never reach the durable database"
    );
    let work = receiver.recv().await.unwrap();
    assert_eq!(work.interaction_token, secret);
    assert_eq!(
        std::fs::canonicalize(work.custody_database).unwrap(),
        std::fs::canonicalize(database.path()).unwrap()
    );
}

#[tokio::test]
async fn busy_click_freezes_authorization_and_coalesces_across_interaction_ids() {
    let database = DispatchDatabase::new();
    let choice_id = create_busy_choice(
        database.path(),
        NewBusyChoice {
            owner_user_id: 20,
            channel_id: 10,
            target_thread_id: Some("thread-a"),
            prompt: "preserve this exact prompt",
            allow_steer: true,
            now: now(),
            time_to_live: 1_800.0,
        },
    )
    .unwrap();
    let transport = Arc::new(InspectingTransport::new(database.path()));
    let (sender, mut receiver) = mpsc::channel(2);
    let dispatcher = InteractionDispatcher::new(
        Arc::clone(&transport),
        policy(),
        false,
        sender,
        database.path(),
    );

    assert_eq!(
        dispatcher
            .dispatch(
                &busy_component(902, "busy-token", &choice_id, "queue"),
                Instant::now(),
                InteractionIngressTag::Normal,
            )
            .await
            .unwrap(),
        DispatchOutcome::Queued
    );
    {
        let seen = transport.seen_at_ack.lock().unwrap();
        assert_eq!(seen[0].payload["busy_action"], "queue");
        assert_eq!(
            seen[0].payload["busy_choice"]["prompt"],
            "preserve this exact prompt"
        );
    }
    let work = receiver.recv().await.unwrap();
    let frozen = work.authorized_busy_choice.unwrap();
    assert_eq!(frozen.choice_id, choice_id);
    assert_eq!(frozen.prompt, "preserve this exact prompt");
    assert_eq!(frozen.target_thread_id.as_deref(), Some("thread-a"));
    let stored = cdr_store::ingress::by_origin(database.path(), 902)
        .unwrap()
        .unwrap();
    let canonical_owner = format!("busy-choice:{choice_id}");
    assert_eq!(
        stored.canonical_owner.as_deref(),
        Some(canonical_owner.as_str())
    );
    assert_eq!(stored.payload["busy_action"], "queue");
    assert_eq!(
        stored.payload["busy_choice"]["prompt"],
        "preserve this exact prompt"
    );

    assert_eq!(
        dispatcher
            .dispatch(
                &busy_component(903, "second-token", &choice_id, "queue"),
                Instant::now(),
                InteractionIngressTag::Normal,
            )
            .await
            .unwrap(),
        DispatchOutcome::RespondedWithoutWork
    );
    assert!(receiver.try_recv().is_err());
    let responses = transport.responses.lock().unwrap();
    assert_eq!(responses.len(), 2);
    assert_eq!(
        responses[1].kind,
        twilight_model::http::interaction::InteractionResponseType::DeferredUpdateMessage
    );
    assert!(
        responses[1].data.is_none(),
        "duplicate clicks must not add a message"
    );
    drop(responses);
    let duplicate = cdr_store::ingress::by_origin(database.path(), 903)
        .unwrap()
        .unwrap();
    assert_eq!(duplicate.phase, "canonical_duplicate");
    assert_eq!(duplicate.owner_kind.as_deref(), Some("ingress"));
}

#[tokio::test]
async fn unavailable_busy_choice_gets_visible_nonfatal_rejection() {
    let database = DispatchDatabase::new();
    let transport = Arc::new(InspectingTransport::new(database.path()));
    let (sender, mut receiver) = mpsc::channel(1);
    let dispatcher = InteractionDispatcher::new(
        Arc::clone(&transport),
        policy(),
        false,
        sender,
        database.path(),
    );

    assert_eq!(
        dispatcher
            .dispatch(
                &busy_component(904, "missing-token", "0123456789abcdef01234567", "queue"),
                Instant::now(),
                InteractionIngressTag::Normal,
            )
            .await
            .unwrap(),
        DispatchOutcome::RespondedWithoutWork
    );
    let response = transport.responses.lock().unwrap();
    let content = response[0]
        .data
        .as_ref()
        .and_then(|data| data.content.as_deref())
        .unwrap();
    assert!(content.contains("no longer active"));
    assert!(receiver.try_recv().is_err());
}
