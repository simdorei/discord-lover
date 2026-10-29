use super::*;
use crate::mirror_sync::{MirrorChannel, MirrorFuture, MirrorInventoryThread, MirrorTransport};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;
use twilight_model::channel::ChannelType;
struct HeldDelete {
    entered: Notify,
    release: Notify,
    calls: AtomicUsize,
    reject: bool,
}
impl MirrorTransport for HeldDelete {
    fn delete(&self, _: u64) -> MirrorFuture<'_, ()> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            self.release.notified().await;
            if self.reject {
                Err(MirrorSyncError::DeleteRejected("fixture rejection".into()))
            } else {
                Ok(())
            }
        })
    }
    fn channel(&self, id: u64) -> MirrorFuture<'_, Option<MirrorChannel>> {
        Box::pin(async move {
            Ok(Some(MirrorChannel {
                id,
                guild_id: Some(1),
                parent_id: None,
                kind: ChannelType::GuildText,
                name: "codex".into(),
                topic: None,
                archived: false,
            }))
        })
    }
    fn channels(&self, _: u64) -> MirrorFuture<'_, Vec<MirrorChannel>> {
        unreachable!()
    }
    fn thread_inventory(&self, _: u64, _: u64) -> MirrorFuture<'_, Vec<MirrorInventoryThread>> {
        unreachable!()
    }
    fn create<'a>(
        &'a self,
        _: u64,
        _: Option<u64>,
        _: ChannelType,
        _: &'a str,
        _: Option<&'a str>,
    ) -> MirrorFuture<'a, MirrorChannel> {
        unreachable!()
    }
    fn update<'a>(
        &'a self,
        _: &'a MirrorChannel,
        _: &'a str,
        _: Option<&'a str>,
        _: bool,
    ) -> MirrorFuture<'a, ()> {
        unreachable!()
    }
}
#[tokio::test]
async fn custody_loss_after_delete_reports_dispatch_and_retains_fence_and_mapping() {
    for reject in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mirror = temp.path().join("mirror.sqlite");
        cdr_store::mapping::upsert_thread(&mirror, "codex-a", "C:/test", "test", 30, 31, 1.0)
            .unwrap();
        let _agy =
            crate::agy::store::Store::open(&temp.path().join(".codex_discord_agy.sqlite"), 1)
                .unwrap();
        let remote = Arc::new(HeldDelete {
            entered: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
            reject,
        });
        let sync = MirrorSynchronizer::new(
            temp.path().join("state.sqlite"),
            mirror.clone(),
            remote.clone(),
            Some(1),
        );
        let topology = cdr_discord::gateway::topology::GatewayTopology::default();
        sync.ownership.attach_topology(topology.clone()).unwrap();
        let custody = sync.custody(31).await.unwrap();
        let deletion = sync.delete_guarded(31, Some("codex-a"), &custody);
        let revoke = async {
            remote.entered.notified().await;
            topology.invalidate();
            remote.release.notify_one();
        };
        let (result, ()) = tokio::join!(deletion, revoke);
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("dispatched=true") && error.contains("room 31"),
            "{error}"
        );
        assert!(!error.contains("no deletion was dispatched"));
        assert!(error.contains(if reject {
            "remote rejected deletion"
        } else {
            "remote reported success"
        }));
        assert_eq!(remote.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            cdr_store::room_cleanup::phase(&mirror, 31)
                .unwrap()
                .as_deref(),
            Some("deleting")
        );
        assert_eq!(
            cdr_store::mapping::mirrored_thread_id(&mirror, Some(31))
                .unwrap()
                .as_deref(),
            Some("codex-a")
        );
        assert!(sync.ensure_cleanup_reconciled().is_err());
    }
}
