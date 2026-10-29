use super::*;
use crate::agy::ownership::{Owner, mark_configured, read_snapshot};

#[tokio::test]
async fn historical_stop_cannot_cancel_a_new_generation_job() {
    struct OldStop(u64);
    impl crate::agy::history::History for OldStop {
        fn recent(&self, _: u64) -> crate::agy::history::HistoryFuture<'_> {
            Box::pin(async move { Ok(vec![message(99, self.0, "!stop")]) })
        }
    }
    let mut fixture = fixture().await;
    Arc::get_mut(&mut fixture.service).unwrap().history = Arc::new(OldStop(fixture.thread));
    fixture
        .service
        .open_admission(2, "next".into(), 100)
        .await
        .unwrap();
    fixture
        .service
        .handle_message(&message(101, fixture.thread, "new work"), Some(9))
        .await
        .unwrap();
    let job = fixture.receiver.recv().await.unwrap();
    assert!(
        fixture
            .service
            .recover_gap(fixture.thread, 99, 9)
            .await
            .unwrap()
    );
    assert!(!*job.stop.borrow(), "old history stop cancelled new work");
    fixture
        .service
        .handle_message(&message(102, fixture.thread, "!stop"), Some(9))
        .await
        .unwrap();
    assert!(*job.stop.borrow(), "new stop must still work");
}

#[tokio::test]
async fn off_cannot_cross_actual_claim_commit_or_enqueue_boundaries() {
    use crate::agy::worker::SubmitBarrier;
    use crate::restart_readiness::drain::DrainFenceKey;
    for point in 0..3 {
        let mut f = fixture().await;
        let barrier = Arc::new(SubmitBarrier {
            point,
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        Arc::get_mut(&mut f.service).unwrap().submit_barrier = Some(Arc::clone(&barrier));
        let service = Arc::clone(&f.service);
        let channel = f.thread;
        let submit = tokio::spawn(async move {
            service
                .submit(
                    "message:101",
                    channel,
                    Room::Session(channel),
                    "work".into(),
                    false,
                )
                .await
        });
        barrier.entered.notified().await;
        let service = Arc::clone(&f.service);
        let off = tokio::spawn(async move {
            service.close_admission().await;
        });
        tokio::task::yield_now().await;
        assert!(!off.is_finished());
        let key = DrainFenceKey::new("fixture-runtime", "1|2", "fixture-nonce").unwrap();
        f.service.admission.seal(&key).unwrap();
        assert!(!f.service.admission.is_drained_for(&key));
        barrier.release.notify_one();
        assert!(submit.await.unwrap().is_ok());
        off.await.unwrap();
        let job = f.receiver.try_recv().unwrap();
        assert!(f.receiver.try_recv().is_err());
        assert_eq!(
            f.service
                .store
                .last_result(channel)
                .await
                .unwrap()
                .unwrap()
                .0,
            "accepted"
        );
        assert!(!f.service.quiescent());
        f.service
            .store
            .finish(&job.id, "interrupted", "test shutdown")
            .await
            .unwrap();
        drop(job);
        assert!(f.service.admission.is_drained_for(&key));
        assert!(f.service.quiescent());
        assert!(f.service.admission.release(&key));
        f.service
            .submit(
                "message:102",
                channel,
                Room::Session(channel),
                "after Off".into(),
                false,
            )
            .await
            .unwrap();
        assert!(f.receiver.try_recv().is_err());
    }
}

#[tokio::test]
async fn custody_read_failure_preserves_verified_codex_execution_but_never_cleanup() {
    let fixture = fixture().await;
    let owner = &fixture.service.ownership;
    owner
        .observe_remote(&crate::mirror_sync::MirrorChannel {
            id: 501,
            guild_id: Some(1),
            parent_id: None,
            kind: twilight_model::channel::ChannelType::GuildText,
            name: "unknown".into(),
            topic: None,
            archived: false,
        })
        .unwrap();
    assert_eq!(owner.classify(501), Owner::Unmanaged);
    owner.creation_pending("first").unwrap();
    owner.creation_pending("second").unwrap();
    owner.creations_settled("second").unwrap();
    assert_eq!(owner.classify(501), Owner::Uncertain);
    assert_eq!(owner.classify(90), Owner::Codex);
    assert!(owner.cleanup(90, &fixture.service.remote).await.is_err());
    owner.creations_settled("first").unwrap();
    owner.unavailable();
    assert_eq!(owner.classify(90), Owner::Codex);
    assert_eq!(owner.classify(fixture.thread), Owner::Agy);
    assert_eq!(owner.classify(501), Owner::Uncertain);
    assert!(owner.cleanup(90, &fixture.service.remote).await.is_err());
    let topology = cdr_discord::gateway::topology::GatewayTopology::default();
    owner.attach_topology(topology.clone()).unwrap();
    topology.invalidate();
    assert_eq!(
        owner.classify(90),
        Owner::Uncertain,
        "reconnection invalidates previous negative proof"
    );
}
#[tokio::test]
async fn custody_read_does_not_recover_requests_and_missing_marked_database_is_not_empty() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".codex_discord_agy.sqlite");
    mark_configured(temp.path(), 1).unwrap();
    let store = Store::open(&path, 1).unwrap();
    store.create_session(100, "test", temp.path()).unwrap();
    store.save_conversation(100, "native").unwrap();
    store.claim("message:1", 100).unwrap();
    store.running("message:1", 100).unwrap();
    let before = store.last_result(100).unwrap();
    let snapshot = read_snapshot(temp.path(), &temp.path().join("mirror.sqlite"), 1).unwrap();
    assert!(snapshot.agy.contains_key(&100));
    assert_eq!(store.last_result(100).unwrap(), before);
    drop(store);
    std::fs::rename(&path, temp.path().join("preserved.sqlite")).unwrap();
    assert!(read_snapshot(temp.path(), &temp.path().join("mirror.sqlite"), 1).is_err());
    std::fs::write(&path, b"corrupt").unwrap();
    assert!(read_snapshot(temp.path(), &temp.path().join("mirror.sqlite"), 1).is_err());
}
#[tokio::test]
async fn custody_positive_ids_conflicts_moves_and_reconnect_loss_are_protected() {
    let fixture = fixture().await;
    let owner = &fixture.service.ownership;
    assert_eq!(owner.classify(fixture.thread), Owner::Agy);
    assert_eq!(owner.classify(90), Owner::Codex);
    owner.remember(90, None).unwrap();
    assert_eq!(owner.classify(90), Owner::Conflict);
    let topology = cdr_discord::gateway::topology::GatewayTopology::default();
    owner.attach_topology(topology.clone()).unwrap();
    owner
        .observe_remote(&crate::mirror_sync::MirrorChannel {
            id: 500,
            guild_id: Some(1),
            parent_id: None,
            kind: twilight_model::channel::ChannelType::GuildText,
            name: "room".into(),
            topic: None,
            archived: false,
        })
        .unwrap();
    assert_eq!(owner.classify(500), Owner::Unmanaged);
    topology.invalidate();
    assert_eq!(owner.classify(500), Owner::Uncertain);
    assert_eq!(owner.classify(fixture.thread), Owner::Agy);
    assert!(
        owner
            .cleanup(fixture.thread, &fixture.service.remote)
            .await
            .is_err()
    );
    assert!(owner.cleanup(90, &fixture.service.remote).await.is_err());
}
#[tokio::test]
async fn cutoff_commit_and_off_share_one_acceptance_boundary() {
    let mut fixture = fixture().await;
    let service = &fixture.service;
    service
        .open_admission(2, "epoch".into(), 100)
        .await
        .unwrap();
    service
        .handle_message(&message(100, fixture.thread, "before"), Some(9))
        .await
        .unwrap();
    assert!(fixture.receiver.try_recv().is_err());
    service
        .handle_message(&message(101, fixture.thread, "after"), Some(9))
        .await
        .unwrap();
    let job = fixture.receiver.try_recv().unwrap();
    service.close_admission().await;
    assert!(!service.quiescent());
    service
        .handle_message(&message(102, fixture.thread, "after off"), Some(9))
        .await
        .unwrap();
    assert!(fixture.receiver.try_recv().is_err());
    assert!(
        service.open_admission(3, "next".into(), 102).await.is_err(),
        "accepted request must settle before recovery"
    );
    service
        .store
        .finish(&job.id, "interrupted", "test stop")
        .await
        .unwrap();
    drop(job);
    service.open_admission(3, "next".into(), 102).await.unwrap();
    service
        .handle_message(&message(102, fixture.thread, "old replay"), Some(9))
        .await
        .unwrap();
    assert!(fixture.receiver.try_recv().is_err());
    assert!(
        service
            .submit(
                "interaction:103",
                fixture.thread,
                Room::Session(fixture.thread),
                "new button click".into(),
                false
            )
            .await
            .is_ok()
    );
    assert_eq!(fixture.receiver.try_recv().unwrap().id, "interaction:103");
}
