use std::sync::Arc;

use cdr_discord::gateway::ingress::InteractionIngressTag;
use cdr_runtime::component_worker::busy_ready_marker;
use cdr_runtime::discord_dispatch::{
    DispatchOutcome, InteractionDispatcher, InteractionProcessingMode,
};
use cdr_store::claims::{NewBusyChoice, create_busy_choice};
use cdr_store::prompt_intake::{admit_busy_queue, list_prompt_intakes};
use tokio::{sync::mpsc, time::Instant};

#[path = "support/interaction_custody.rs"]
mod custody_support;
use custody_support::{InspectingTransport, busy_component, now, policy};
#[path = "support/interaction_dispatch.rs"]
mod dispatch_support;
use dispatch_support::DispatchDatabase;

fn create_choice(database: &std::path::Path) -> String {
    create_busy_choice(
        database,
        NewBusyChoice {
            owner_user_id: 20,
            channel_id: 10,
            target_thread_id: Some("thread-a"),
            prompt: "queue this only once",
            allow_steer: true,
            now: now(),
            time_to_live: 1_800.0,
        },
    )
    .unwrap()
}

#[tokio::test]
async fn new_event_for_ready_busy_owner_is_acked_for_confirmation_only() {
    let database = DispatchDatabase::new();
    let choice_id = create_choice(database.path());
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
                &busy_component(951, "first-token", &choice_id, "queue"),
                Instant::now(),
                InteractionIngressTag::Normal,
            )
            .await
            .unwrap(),
        DispatchOutcome::Queued
    );
    let first = receiver.recv().await.unwrap();
    let choice = first.authorized_busy_choice.unwrap();
    let ready_marker = busy_ready_marker(&choice_id, 20, 10);
    let accepted = admit_busy_queue(
        database.path(),
        &choice,
        "thread-a",
        false,
        &ready_marker,
        now(),
    )
    .unwrap();
    assert!(accepted.intake.is_some());
    assert_eq!(list_prompt_intakes(database.path()).unwrap().len(), 1);

    assert_eq!(
        dispatcher
            .dispatch(
                &busy_component(952, "retry-token", &choice_id, "queue"),
                Instant::now(),
                InteractionIngressTag::Normal,
            )
            .await
            .unwrap(),
        DispatchOutcome::Queued,
        "a new event may retry only the durable confirmation"
    );
    let retry = receiver.recv().await.unwrap();
    assert_eq!(retry.interaction_id.get(), 952);
    assert_eq!(
        retry.processing_mode,
        InteractionProcessingMode::ConfirmationOnly
    );
    let retry_choice = retry.authorized_busy_choice.as_ref().unwrap();
    assert_eq!(retry_choice.choice_id, choice.choice_id);
    assert_eq!(retry_choice.owner_user_id, choice.owner_user_id);
    assert_eq!(retry_choice.channel_id, choice.channel_id);
    assert_eq!(retry_choice.target_thread_id, choice.target_thread_id);
    assert_eq!(retry_choice.prompt, choice.prompt);
    assert_eq!(retry_choice.allow_steer, choice.allow_steer);
    assert_eq!(list_prompt_intakes(database.path()).unwrap().len(), 1);
    assert_eq!(transport.responses.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn new_event_for_unowned_saved_choice_is_quiet_and_keeps_one_manual_review_notice() {
    let database = DispatchDatabase::new();
    let choice_id = create_choice(database.path());
    let transport = Arc::new(InspectingTransport::new(database.path()));
    transport.set_fail_ack(true);
    let (sender, mut receiver) = mpsc::channel(1);
    let dispatcher = InteractionDispatcher::new(
        Arc::clone(&transport),
        policy(),
        false,
        sender,
        database.path(),
    );

    assert!(
        dispatcher
            .dispatch(
                &busy_component(953, "failed-token", &choice_id, "queue"),
                Instant::now(),
                InteractionIngressTag::Normal,
            )
            .await
            .is_err()
    );
    assert!(receiver.try_recv().is_err());
    let parent = cdr_store::ingress::by_origin(database.path(), 953)
        .unwrap()
        .unwrap();
    assert_eq!(parent.state, "held");
    let notices_before = cdr_store::delivery::list_pending(database.path()).unwrap();
    assert_eq!(notices_before.len(), 1);
    assert!(
        notices_before[0]
            .content
            .contains("Saved Discord request requires review")
    );
    assert!(notices_before[0].content.contains(&parent.ingress_id));

    transport.set_fail_ack(false);
    assert_eq!(
        dispatcher
            .dispatch(
                &busy_component(954, "review-token", &choice_id, "queue"),
                Instant::now(),
                InteractionIngressTag::Normal,
            )
            .await
            .unwrap(),
        DispatchOutcome::RespondedWithoutWork
    );
    assert!(receiver.try_recv().is_err());
    let repeat = cdr_store::ingress::by_origin(database.path(), 954)
        .unwrap()
        .unwrap();
    assert_eq!(repeat.phase, "canonical_duplicate");
    assert_eq!(repeat.owner_kind.as_deref(), Some("ingress"));
    assert_eq!(repeat.owner_id.as_deref(), Some("interaction:953"));
    let responses = transport.responses.lock().unwrap();
    assert_eq!(responses.len(), 2);
    assert_eq!(
        responses[1].kind,
        twilight_model::http::interaction::InteractionResponseType::DeferredUpdateMessage
    );
    assert!(responses[1].data.is_none());
    let notices_after = cdr_store::delivery::list_pending(database.path()).unwrap();
    assert_eq!(notices_after.len(), 1);
    assert_eq!(notices_after[0].delivery_id, notices_before[0].delivery_id);
    assert_eq!(notices_after[0].content, notices_before[0].content);
    assert!(list_prompt_intakes(database.path()).unwrap().is_empty());
    assert!(cdr_store::queue::list(database.path()).unwrap().is_empty());
}
