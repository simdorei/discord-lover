use super::revision_contracts::agy_interaction;
use super::*;
use twilight_model::http::interaction::InteractionResponseType;

async fn wait_acks(acks: &FakeInteractions, count: usize, at: Instant) {
    tokio::time::timeout(Duration::from_millis(2500), async {
        while acks.acknowledgments.lock().unwrap().len() < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let observed = acks.acknowledgments.lock().unwrap().clone();
    assert_eq!(observed.len(), count);
    for (_, when, kind) in observed {
        assert!(when.duration_since(at) <= Duration::from_millis(2500));
        assert!(matches!(
            kind,
            InteractionResponseType::ChannelMessageWithSource
                | InteractionResponseType::ApplicationCommandAutocompleteResult
        ));
    }
    assert_eq!(acks.updates.load(Ordering::SeqCst), 0);
}
fn assert_unclaimed(f: &crate::agy::tests::support::Fixture) {
    let db = rusqlite::Connection::open(f.temp.path().join(".codex_discord_agy.sqlite")).unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    assert!(f.service.quiescent());
}

#[tokio::test]
async fn fast_unknown_lookup_error_and_panic_ack_each_type_and_allow_next_codex() {
    for behavior in [1, 2] {
        let mut f = fixture().await;
        f.remote.behavior.store(behavior, Ordering::SeqCst);
        let acks = Arc::new(FakeInteractions::default());
        let (manager, _status, mut failures) = routing_handle(
            Some(Arc::clone(&f.service)),
            Arc::clone(&f.service.ownership),
            Phase::Ready,
        );
        let (stop, rx) = watch::channel(false);
        let lanes = AgyRouting::start_with_adapters(
            manager,
            Arc::clone(&f.service.policy),
            acks.clone(),
            rx,
            Arc::clone(&f.service.remote),
            f.messages.clone(),
        );
        let (gateway, receivers) = GatewayIngress::new(GatewayIngressConfig::default()).unwrap();
        let (samples, mut observed) = mpsc::unbounded_channel();
        let (done, done_rx) = watch::channel(false);
        let common = tokio::spawn(crate::discord_runtime::run_agy_performance_lane(
            receivers.normal_interactions,
            Some(lanes.routing.clone()),
            samples,
            done_rx,
        ));
        let at = Instant::now();
        for kind in 0..3 {
            let _ = gateway.publish(
                Event::InteractionCreate(Box::new(agy_interaction(
                    1000 + kind,
                    999,
                    usize::try_from(kind).unwrap(),
                ))),
                Instant::now(),
            );
        }
        wait_acks(&acks, 3, at).await;
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(1), failures.recv())
                .await
                .unwrap()
                .unwrap();
        }
        let _ = gateway.publish(
            Event::InteractionCreate(Box::new(interaction(2000))),
            Instant::now(),
        );
        let sample = tokio::time::timeout(Duration::from_millis(500), observed.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sample.0, 2000);
        assert!(f.receiver.try_recv().is_err());
        assert_unclaimed(&f);
        assert!(!common.is_finished());
        done.send(true).unwrap();
        common.await.unwrap().unwrap();
        stop.send(true).unwrap();
        lanes
            .join(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn full_codex_staging_does_not_hold_agy_ack_in_normal_or_reserved_consumer() {
    for reserved in [false, true] {
        let mut f = fixture().await;
        let acks = Arc::new(FakeInteractions::default());
        let (manager, _status, _failures) =
            routing_handle(None, Arc::clone(&f.service.ownership), Phase::Off);
        let (stop, rx) = watch::channel(false);
        let lanes = AgyRouting::start_with_adapters(
            manager,
            Arc::clone(&f.service.policy),
            acks.clone(),
            rx,
            Arc::clone(&f.service.remote),
            f.messages.clone(),
        );
        let (gateway, receivers) = GatewayIngress::new(GatewayIngressConfig {
            interaction_capacity: 256,
            reserved_interaction_capacity: 256,
            ..Default::default()
        })
        .unwrap();
        if reserved {
            gateway.stop_accepting();
        }
        let (samples, mut observed) = mpsc::unbounded_channel();
        let (done, done_rx) = watch::channel(false);
        let held = Arc::new(tokio::sync::Semaphore::new(0));
        let receiver = if reserved {
            receivers.reserved_interactions
        } else {
            receivers.normal_interactions
        };
        let common = tokio::spawn(crate::discord_runtime::run_agy_probe_lane(
            receiver,
            Some(lanes.routing.clone()),
            samples,
            done_rx,
            Some(Arc::clone(&held)),
            reserved,
        ));
        for id in 1000..1128 {
            let _ = gateway.publish(
                Event::InteractionCreate(Box::new(interaction(id))),
                Instant::now(),
            );
        }
        let active = if reserved { 4 } else { 16 };
        let mut started = 0;
        let mut rejected = 0;
        tokio::time::timeout(Duration::from_secs(1), async {
            while rejected < 128 - active - 64 {
                let (_, kind, _) = observed.recv().await.unwrap();
                if kind == 1 {
                    started += 1;
                } else if kind == 9 {
                    rejected += 1;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(started, active, "execution concurrency changed");
        let at = Instant::now();
        for kind in 0..3 {
            let _ = gateway.publish(
                Event::InteractionCreate(Box::new(agy_interaction(
                    3000 + kind,
                    f.thread,
                    usize::try_from(kind).unwrap(),
                ))),
                Instant::now(),
            );
        }
        wait_acks(&acks, 3, at).await;
        assert_eq!(
            held.available_permits(),
            0,
            "Codex jobs were released before ACK proof"
        );
        assert!(f.receiver.try_recv().is_err());
        assert_unclaimed(&f);
        assert!(!common.is_finished());
        held.add_permits(128);
        drop(gateway);
        done.send(true).unwrap();
        common.await.unwrap().unwrap();
        stop.send(true).unwrap();
        lanes
            .join(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }
}
