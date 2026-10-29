use super::*;
use crate::mirror_sync::{MirrorChannel, MirrorFuture, MirrorInventoryThread, MirrorTransport};
use cdr_codex_state::CodexThreadStore;
use cdr_store::mapping::{self, StaleThread};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use twilight_model::channel::ChannelType;

struct Rooms {
    rooms: Mutex<BTreeMap<u64, MirrorChannel>>,
    deletes: AtomicUsize,
}
impl MirrorTransport for Rooms {
    fn delete(&self, id: u64) -> MirrorFuture<'_, ()> {
        Box::pin(async move {
            self.deletes.fetch_add(1, Ordering::SeqCst);
            self.rooms.lock().unwrap().remove(&id);
            Ok(())
        })
    }
    fn channel(&self, id: u64) -> MirrorFuture<'_, Option<MirrorChannel>> {
        Box::pin(async move { Ok(self.rooms.lock().unwrap().get(&id).cloned()) })
    }
    fn channels(&self, _: u64) -> MirrorFuture<'_, Vec<MirrorChannel>> {
        unreachable!()
    }
    fn thread_inventory(&self, _: u64, _: u64) -> MirrorFuture<'_, Vec<MirrorInventoryThread>> {
        Box::pin(async { Ok(Vec::new()) })
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
#[allow(
    clippy::too_many_lines,
    reason = "the ordered fixture keeps each caller's completed fence and retained mapping evidence together"
)]
async fn actual_cleanup_callers_preserve_confirmed_result_when_retirement_loses_custody() {
    for caller in 0..4 {
        for missing in [false, true] {
            if caller == 3 && !missing {
                continue;
            }
            let temp = tempfile::tempdir().unwrap();
            let state = temp.path().join("state.sqlite");
            let mirror = temp.path().join("mirror.sqlite");
            let db = rusqlite::Connection::open(&state).unwrap();
            db.execute_batch(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/action_state.sql"
            )))
            .unwrap();
            let rollout = temp.path().join("old.jsonl");
            std::fs::write(&rollout, "").unwrap();
            db.execute(
                "UPDATE threads SET rollout_path=?",
                [rollout.to_string_lossy().as_ref()],
            )
            .unwrap();
            if caller == 3 {
                db.execute("DELETE FROM threads WHERE id='thread-old'", [])
                    .unwrap();
            }
            mapping::upsert_project(&mirror, "C:/old", "old", 21, 0.0, |a, b| a == b).unwrap();
            if caller != 2 {
                mapping::upsert_thread(&mirror, "thread-old", "C:/old", "old", 21, 31, 0.0)
                    .unwrap();
            }
            let id = if caller == 2 { 21 } else { 31 };
            let rooms = [
                MirrorChannel {
                    id: 21,
                    guild_id: Some(1),
                    parent_id: None,
                    kind: ChannelType::GuildText,
                    name: "old".into(),
                    topic: None,
                    archived: false,
                },
                MirrorChannel {
                    id: 31,
                    guild_id: Some(1),
                    parent_id: Some(21),
                    kind: ChannelType::PublicThread,
                    name: "old".into(),
                    topic: None,
                    archived: false,
                },
            ]
            .into_iter()
            .filter(|c| !(missing && c.id == id))
            .map(|c| (c.id, c))
            .collect();
            let remote = Arc::new(Rooms {
                rooms: Mutex::new(rooms),
                deletes: AtomicUsize::new(0),
            });
            let _agy =
                crate::agy::store::Store::open(&temp.path().join(".codex_discord_agy.sqlite"), 1)
                    .unwrap();
            let mut sync =
                MirrorSynchronizer::new(state.clone(), mirror.clone(), remote.clone(), Some(1));
            let topology = cdr_discord::gateway::topology::GatewayTopology::default();
            sync.ownership.attach_topology(topology.clone()).unwrap();
            if caller == 3 {
                let token =
                    cdr_store::room_cleanup::begin(&mirror, 31, Some("thread-old"), 1.0).unwrap();
                cdr_store::room_cleanup::complete(&mirror, 31, &token).unwrap();
            }
            let barriers = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&barriers);
            let fence = mirror.clone();
            sync.before_retirement = Some(Arc::new(move |room| {
                assert_eq!(room, id);
                assert_eq!(
                    cdr_store::room_cleanup::phase(&fence, i64::try_from(room).unwrap())
                        .unwrap()
                        .as_deref(),
                    Some("deleted")
                );
                observed.fetch_add(1, Ordering::SeqCst);
                topology.invalidate();
            }));
            let store = CodexThreadStore::open(&state).unwrap();
            let row = StaleThread {
                thread_id: "thread-old".into(),
                discord_thread_id: 31,
                title: "old".into(),
            };
            let result = match caller {
                0 => sync
                    .retire_obsolete_row(&store, 1, 10.0, &row, None)
                    .await
                    .map(|_| ()),
                1 => sync
                    .retire_archived_rejections(
                        &store,
                        1,
                        10.0,
                        &row,
                        &store.load_thread("thread-old", true).unwrap().unwrap(),
                    )
                    .await
                    .map(|_| ()),
                2 => sync.cleanup_orphans(1, 10.0).await.map(|_| ()),
                _ => sync.retire_exact_absent("thread-old", 31, 21).await,
            };
            let error = result.unwrap_err().to_string();
            assert_eq!(
                barriers.load(Ordering::SeqCst),
                1,
                "{caller}/{missing}: {error}"
            );
            assert!(error.contains(&format!("room {id}")), "{error}");
            assert!(
                !error.contains("no deletion was dispatched for this room"),
                "{error}"
            );
            assert!(
                error.contains(if caller == 3 {
                    "previously confirmed cleanup"
                } else if missing {
                    "remote absence observed without deletion"
                } else {
                    "dispatched=true; remote reported success"
                }),
                "{error}"
            );
            if missing {
                assert!(!error.contains("dispatched=true"));
            }
            assert_eq!(remote.deletes.load(Ordering::SeqCst), usize::from(!missing));
            assert_eq!(
                cdr_store::room_cleanup::phase(&mirror, i64::try_from(id).unwrap())
                    .unwrap()
                    .as_deref(),
                Some("deleted")
            );
            if caller == 2 {
                assert!(
                    mapping::remaining_discord_ids(&mirror)
                        .unwrap()
                        .project_channel_ids
                        .contains(&21)
                );
            } else {
                assert_eq!(
                    mapping::mirrored_thread_id(&mirror, Some(31))
                        .unwrap()
                        .as_deref(),
                    Some("thread-old")
                );
            }
        }
    }
}
