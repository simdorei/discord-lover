use super::*;
use crate::agy::Room;
use twilight_model::application::interaction::InteractionType;

#[tokio::test]
async fn common_history_and_interaction_failures_do_not_notify_runtime_fatal_exit() {
    struct PanicHistory;
    impl crate::agy::history::History for PanicHistory {
        fn recent(&self, _: u64) -> crate::agy::history::HistoryFuture<'_> {
            Box::pin(async { panic!("fixture history panic") })
        }
    }
    for behavior in [1, 2] {
        let mut f = fixture().await;
        f.remote.behavior.store(behavior, Ordering::SeqCst);
        if behavior == 2 {
            Arc::get_mut(&mut f.service).unwrap().history = Arc::new(PanicHistory);
        }
        let (manager, _status, failures) = routing_handle(
            Some(Arc::clone(&f.service)),
            Arc::clone(&f.service.ownership),
            Phase::Ready,
        );
        let (stop, rx) = watch::channel(false);
        let acknowledgments = Arc::new(FakeInteractions::default());
        let lanes = AgyRouting::start_with_adapters(
            manager,
            Arc::clone(&f.service.policy),
            acknowledgments.clone(),
            rx,
            Arc::clone(&f.service.remote),
            f.messages.clone(),
        );
        crate::discord_runtime::assert_common_consumers_survive(
            lanes.routing.clone(),
            Arc::clone(&f.service.policy),
            failures,
            f.thread,
        )
        .await;
        let observed = acknowledgments.acknowledgments.lock().unwrap().clone();
        assert_eq!(
            observed.len(),
            3,
            "every lookup failure/panic must ACK once"
        );
        assert_eq!(acknowledgments.updates.load(Ordering::SeqCst), 0);
        assert!(f.receiver.try_recv().is_err());
        stop.send(true).unwrap();
        lanes
            .join(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        assert!(f.service.quiescent());
    }
}

async fn wait_count(counter: &AtomicUsize, minimum: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while counter.load(Ordering::SeqCst) < minimum {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[allow(deprecated, reason = "test includes older Discord payloads")]
pub(super) fn agy_interaction(id: u64, channel: u64, kind: usize) -> InteractionCreate {
    let mut item = interaction(id);
    item.0.channel_id = Some(twilight_model::id::Id::new(channel));
    if kind == 1 {
        item.0.kind = InteractionType::ApplicationCommandAutocomplete;
    }
    if kind == 2 {
        item.0.kind = InteractionType::MessageComponent;
        item.0.data = None;
    }
    item
}
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one ordered recovery or race scenario keeps custody boundaries visible"
)]
async fn known_agy_ack_deadlines_survive_held_status_final_edits_and_full_lookup() {
    for hold_final in [false, true] {
        let mut f = fixture().await;
        let held_until = Instant::now() + Duration::from_secs(5);
        let calls = Arc::new(AtomicUsize::new(0));
        let held: Arc<dyn Messenger> = Arc::new(HeldMessages {
            calls: Arc::clone(&calls),
            until: held_until,
        });
        Arc::get_mut(&mut f.service).unwrap().messenger = Arc::clone(&held);
        let acknowledgments = Arc::new(FakeInteractions {
            hold_until: Some(held_until),
            ..Default::default()
        });
        let (manager, _status, _failures) = routing_handle(
            Some(Arc::clone(&f.service)),
            Arc::clone(&f.service.ownership),
            Phase::Ready,
        );
        let (stop, shutdown) = watch::channel(false);
        let lanes_owner = AgyRouting::start_with_adapters(
            manager,
            Arc::clone(&f.service.policy),
            acknowledgments.clone(),
            shutdown,
            Arc::clone(&f.service.remote),
            held,
        );
        let (gateway, mut lanes) = GatewayIngress::new(GatewayIngressConfig::default()).unwrap();
        let (resume, _) = mpsc::channel(32);
        let (samples, mut sample_rx) = mpsc::unbounded_channel();
        let (done, done_rx) = watch::channel(false);
        let common = tokio::spawn(crate::discord_runtime::run_agy_performance_lane(
            lanes.normal_interactions,
            Some(lanes_owner.routing.clone()),
            samples,
            done_rx,
        ));
        if hold_final {
            for i in 0..4 {
                let _ = gateway.publish(
                    Event::InteractionCreate(Box::new(agy_interaction(5001 + i * 4, f.thread, 0))),
                    Instant::now(),
                );
            }
            wait_count(&acknowledgments.updates, 4).await;
        } else {
            for i in 0..4 {
                let _ = gateway.publish(
                    Event::MessageCreate(Box::new(MessageCreate(message(
                        500 + i,
                        f.thread,
                        "!status",
                    )))),
                    Instant::now(),
                );
                lanes_owner.routing.message(
                    lanes.messages.recv().await.unwrap(),
                    9,
                    resume.clone(),
                );
            }
            wait_count(&calls, 4).await;
        }
        // Fill completion and lookup queues through the actual router. Slow
        // ownership lookups cannot consume the initial-ACK execution slots.
        for i in 0..32 {
            let _ = gateway.publish(
                Event::MessageCreate(Box::new(MessageCreate(message(
                    600 + i,
                    f.thread,
                    "!status",
                )))),
                Instant::now(),
            );
            lanes_owner
                .routing
                .message(lanes.messages.recv().await.unwrap(), 9, resume.clone());
        }
        f.remote.behavior.store(3, Ordering::SeqCst);
        for id in 10000..10040 {
            lanes_owner.routing.classify_later(id);
        }
        let at = Instant::now();
        for i in 0..40 {
            let _ = gateway.publish(
                Event::InteractionCreate(Box::new(agy_interaction(
                    8000 + i,
                    f.thread,
                    (i % 3) as usize,
                ))),
                Instant::now(),
            );
        }
        let _ = gateway.publish(
            Event::InteractionCreate(Box::new(interaction(9000))),
            Instant::now(),
        );
        tokio::time::timeout(Duration::from_millis(500), sample_rx.recv())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_millis(2500), async {
            loop {
                if acknowledgments
                    .acknowledgments
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(id, _, _)| *id >= 8000)
                    .count()
                    == 40
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let observations = acknowledgments.acknowledgments.lock().unwrap().clone();
        for (id, ack, _) in observations.iter().filter(|(id, _, _)| *id >= 8000) {
            assert!(
                ack.duration_since(at) <= Duration::from_millis(2500),
                "late ACK {id}"
            );
        }
        assert!(Instant::now() < held_until);
        assert!(
            f.receiver.try_recv().is_err(),
            "overflow/unsupported/autocomplete cannot execute"
        );
        assert!(
            !common.is_finished(),
            "AGY does not publish a common fatal error"
        );
        done.send(true).unwrap();
        common.await.unwrap().unwrap();
        stop.send(true).unwrap();
        lanes_owner
            .join(held_until + Duration::from_secs(3))
            .await
            .unwrap();
        assert!(f.service.quiescent());
    }
}

#[tokio::test]
async fn queued_old_stop_is_not_retargeted_and_fresh_draining_stop_still_works() {
    let mut f = fixture().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let until = Instant::now() + Duration::from_millis(600);
    let held = Arc::new(HeldMessages {
        calls: Arc::clone(&calls),
        until,
    });
    let (manager, status, _) = routing_handle(
        Some(Arc::clone(&f.service)),
        Arc::clone(&f.service.ownership),
        Phase::Off,
    );
    let (stop, rx) = watch::channel(false);
    let lanes_owner = AgyRouting::start_with_adapters(
        manager,
        Arc::clone(&f.service.policy),
        Arc::new(FakeInteractions::default()),
        rx,
        Arc::clone(&f.service.remote),
        held,
    );
    let (gateway, mut lanes) = GatewayIngress::new(GatewayIngressConfig::default()).unwrap();
    let (resume, _) = mpsc::channel(1);
    for id in 80..84 {
        let _ = gateway.publish(
            Event::MessageCreate(Box::new(MessageCreate(message(id, f.thread, "!status")))),
            Instant::now(),
        );
        lanes_owner
            .routing
            .message(lanes.messages.recv().await.unwrap(), 9, resume.clone());
    }
    wait_count(&calls, 4).await;
    let _ = gateway.publish(
        Event::MessageCreate(Box::new(MessageCreate(message(99, f.thread, "!stop")))),
        Instant::now(),
    );
    lanes_owner
        .routing
        .message(lanes.messages.recv().await.unwrap(), 9, resume.clone());
    f.service
        .open_admission(2, "new".into(), 100)
        .await
        .unwrap();
    status.send_modify(|s| {
        s.generation = 2;
        s.phase = Phase::Ready;
    });
    f.service
        .submit(
            "message:101",
            f.thread,
            Room::Session(f.thread),
            "new job".into(),
            false,
        )
        .await
        .unwrap();
    let job = f.receiver.recv().await.unwrap();
    tokio::time::sleep_until(until + Duration::from_millis(60)).await;
    assert!(!*job.stop.borrow(), "queued old stop reached new work");
    status.send_modify(|s| {
        s.generation = 3;
        s.phase = Phase::Draining;
    });
    f.service.close_admission().await;
    let _ = gateway.publish(
        Event::InteractionCreate(Box::new(agy_interaction(104, f.thread, 0))),
        Instant::now(),
    );
    let (interaction_resume, _) = mpsc::channel(1);
    lanes_owner.routing.interaction(
        lanes.normal_interactions.recv().await.unwrap(),
        interaction_resume,
    );
    tokio::time::timeout(Duration::from_millis(500), async {
        while !*job.stop.borrow() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    stop.send(true).unwrap();
    lanes_owner
        .join(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    f.service
        .store
        .finish(&job.id, "stopped", "fixture")
        .await
        .unwrap();
    drop(job);
    assert!(f.service.quiescent());
}

#[tokio::test]
async fn failed_ack_never_claims_or_enqueues_execution() {
    let mut f = fixture().await;
    let acks = Arc::new(FakeInteractions {
        fail_ack: true,
        ..Default::default()
    });
    let (manager, _status, mut failures) = routing_handle(
        Some(Arc::clone(&f.service)),
        Arc::clone(&f.service.ownership),
        Phase::Ready,
    );
    let (stop, rx) = watch::channel(false);
    let lanes_owner = AgyRouting::start_with_adapters(
        manager,
        Arc::clone(&f.service.policy),
        acks,
        rx,
        Arc::clone(&f.service.remote),
        f.messages.clone(),
    );
    let (gateway, mut lanes) = GatewayIngress::new(GatewayIngressConfig::default()).unwrap();
    let _ = gateway.publish(
        Event::InteractionCreate(Box::new(agy_interaction(100, f.thread, 0))),
        Instant::now(),
    );
    let (resume, _) = mpsc::channel(1);
    lanes_owner
        .routing
        .interaction(lanes.normal_interactions.recv().await.unwrap(), resume);
    tokio::time::timeout(Duration::from_millis(500), failures.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(f.receiver.try_recv().is_err());
    assert!(
        f.service
            .store
            .last_result(f.thread)
            .await
            .unwrap()
            .is_none()
    );
    assert!(f.service.quiescent());
    stop.send(true).unwrap();
    lanes_owner
        .join(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
}
