//! The real adapter and cycle/gap runners are used; only network fetch is held.
use super::*;
use crate::{
    app_backend::AppServerTurnBackend,
    history_poll::{
        HistoryPollState, PollStartedAt, run_history_gap_recovery, run_history_poll_cycle,
    },
    test_support::{app_fixture, new_reply_fixture},
};
use cdr_discord::gateway::ingress::{GatewayIngress, GatewayIngressConfig};
use tokio::sync::Notify;
use twilight_model::gateway::{event::Event, payload::incoming::ChannelUpdate};

struct HeldPage<'a> {
    inner: DiscordHistoryCycleIo<'a, AppServerTurnBackend>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}
impl HistoryPollCycleIo for HeldPage<'_> {
    type Payload = Message;
    type Item = HistoryMessagePayload;
    type Admitted = HistoryAdmittedMessage;
    type SourceError = DiscordHistorySourceError;
    type AdaptationError = MessageAdmissionError;
    type ClaimError = MessageAdmissionError;
    type ProcessError = MessageAdmissionError;
    fn fetch(
        &mut self,
        _: u64,
        _: usize,
    ) -> BoxHistoryPollFuture<'_, Vec<Message>, Self::SourceError> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(vec![message()])
        })
    }
    fn adapt(
        &mut self,
        message: Message,
    ) -> Result<HistoryBatchItem<Self::Item>, Self::AdaptationError> {
        self.inner.adapt(message)
    }
    fn claim(
        &mut self,
        item: Self::Item,
        purpose: HistoryClaimPurpose,
    ) -> Result<HistoryClaimOutcome<Self::Admitted>, Self::ClaimError> {
        self.inner.claim(item, purpose)
    }
    fn process(
        &mut self,
        item: Self::Admitted,
    ) -> BoxHistoryPollFuture<'_, (), Self::ProcessError> {
        self.inner.process(item)
    }
}
fn message() -> Message {
    serde_json::from_value(serde_json::json!({
        "attachments":[],"author":{"avatar":null,"bot":false,"discriminator":"0001","id":"3","username":"fixture"},
        "channel_id":"100","content":"!new must never execute","edited_timestamp":null,"embeds":[],"id":"801",
        "mention_everyone":false,"mention_roles":[],"mentions":[],"pinned":false,
        "timestamp":"2020-02-02T02:02:02.020000+00:00","tts":false,"type":0
    })).unwrap()
}
fn move_room(gateway: &GatewayIngress, parent: Option<u64>) {
    let update: ChannelUpdate = serde_json::from_value(serde_json::json!({"id":"100", "guild_id":"1", "parent_id":parent.map(|p|p.to_string()), "type":0, "name":"room"})).unwrap();
    let _ = gateway.publish(
        Event::ChannelUpdate(Box::new(update)),
        tokio::time::Instant::now(),
    );
}

#[tokio::test]
async fn held_periodic_and_gap_fetch_recheck_move_and_epoch_before_any_claim() {
    for gap in [false, true] {
        for invalidate in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let (fixture, remote, replies) =
                new_reply_fixture::setup_with_channels(&temp, vec![100, 43]).await;
            let store =
                crate::agy::store::Store::open(&temp.path().join(".codex_discord_agy.sqlite"), 1)
                    .unwrap();
            store
                .begin_container("category", None, "GuildCategory")
                .unwrap();
            store.confirm_container("category", 900).unwrap();
            let owner = crate::agy::ownership::AgyOwnership::for_root(temp.path(), 1);
            let (gateway, _lanes) = GatewayIngress::new(GatewayIngressConfig::default()).unwrap();
            owner.attach_topology(gateway.topology()).unwrap();
            owner
                .ensure_loaded(fixture.executor.mirror_db())
                .await
                .unwrap();
            move_room(&gateway, None);
            assert!(owner.classify(100).permits_codex());
            let entered = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let mut io = HeldPage {
                inner: DiscordHistoryCycleIo::new(
                    fixture.context(temp.path()),
                    InteractionAccessPolicy {
                        allow_all_channels: true,
                        ..Default::default()
                    },
                    None,
                    SystemTime::now(),
                )
                .with_ownership(Some(Arc::clone(&owner))),
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            };
            let mut state = HistoryPollState::default();
            let floor =
                HistoryWatermark::from_message(message().timestamp.as_micros(), 801).unwrap();
            let run = async {
                if gap {
                    assert!(matches!(
                        run_history_gap_recovery(100, floor, &mut io).await,
                        Err(crate::history_poll::HistoryGapRecoveryError::Claim(
                            MessageAdmissionError::OwnershipChanged { .. }
                        ))
                    ));
                } else {
                    assert!(matches!(
                        run_history_poll_cycle(
                            &mut state,
                            100,
                            PollStartedAt::from_normalized_utc_micros(0),
                            &mut io
                        )
                        .await,
                        Err(crate::history_poll::HistoryPollRunError::Claim(
                            MessageAdmissionError::OwnershipChanged { .. }
                        ))
                    ));
                }
            };
            let change = async {
                entered.notified().await;
                if invalidate {
                    gateway.topology().invalidate();
                } else {
                    move_room(&gateway, Some(900));
                }
                release.notify_one();
            };
            tokio::join!(run, change);
            assert!(state.watermark(100).is_none());
            assert!(
                cdr_store::ingress::get(fixture.executor.mirror_db(), "message:801")
                    .unwrap()
                    .is_none()
            );
            assert!(
                !cdr_store::processed::is_processed(fixture.executor.mirror_db(), 801).unwrap()
            );
            fixture.server.close().await.unwrap();
            replies.stop.send(()).unwrap();
            assert!(replies.task.await.unwrap().is_empty());
            assert_eq!(remote.creates.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(
                app_fixture::rpc_log(&temp.path().join("rpc.jsonl"))
                    .iter()
                    .all(|r| r["method"] != "thread/start" && r["method"] != "turn/start")
            );
        }
    }
}

#[tokio::test]
async fn history_processing_entry_rechecks_after_a_successful_claim() {
    let temp = tempfile::tempdir().unwrap();
    let (fixture, remote, replies) =
        new_reply_fixture::setup_with_channels(&temp, vec![100, 43]).await;
    let _store =
        crate::agy::store::Store::open(&temp.path().join(".codex_discord_agy.sqlite"), 1).unwrap();
    let owner = crate::agy::ownership::AgyOwnership::for_root(temp.path(), 1);
    let (gateway, _) = GatewayIngress::new(GatewayIngressConfig::default()).unwrap();
    owner.attach_topology(gateway.topology()).unwrap();
    owner
        .ensure_loaded(fixture.executor.mirror_db())
        .await
        .unwrap();
    move_room(&gateway, None);
    let mut io = DiscordHistoryCycleIo::new(
        fixture.context(temp.path()),
        InteractionAccessPolicy {
            allow_all_channels: true,
            ..Default::default()
        },
        None,
        SystemTime::now(),
    )
    .with_ownership(Some(owner));
    let HistoryPollItem::Candidate(item) = io.adapt(message()).unwrap().kind else {
        panic!("candidate")
    };
    let HistoryClaimOutcome::Won(item) = io.claim(item, HistoryClaimPurpose::Process).unwrap()
    else {
        panic!("claim")
    };
    gateway.topology().invalidate();
    assert!(matches!(
        io.process(item).await,
        Err(MessageAdmissionError::OwnershipChanged { .. })
    ));
    fixture.server.close().await.unwrap();
    replies.stop.send(()).unwrap();
    assert!(replies.task.await.unwrap().is_empty());
    assert_eq!(remote.creates.load(std::sync::atomic::Ordering::SeqCst), 0);
}
