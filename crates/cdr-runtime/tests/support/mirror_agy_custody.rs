use super::*;
fn protect(root: &std::path::Path, id: u64, corrupt: bool) {
    let path = root.join(".codex_discord_agy.sqlite");
    if corrupt {
        std::fs::write(&path, b"unreadable custody").unwrap();
    } else {
        let store = cdr_runtime::agy::store::Store::open(&path, 1).unwrap();
        store
            .begin_container("protected", None, "GuildText")
            .unwrap();
        store.confirm_container("protected", id).unwrap();
    }
}
#[tokio::test]
async fn custody_blocks_stale_delete_missing_room_fence_and_exact_mapping_retirement() {
    for corrupt in [false, true] {
        for missing in [false, true] {
            for confirmed in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let (sync, remote) = fixture(&temp);
                let db = temp.path().join("mirror.sqlite");
                protect(temp.path(), 31, corrupt);
                if missing {
                    remote.0.lock().unwrap().channels.remove(&31);
                }
                if confirmed {
                    Connection::open(temp.path().join("state.sqlite"))
                        .unwrap()
                        .execute("DELETE FROM threads WHERE id='thread-old'", [])
                        .unwrap();
                    let token =
                        cdr_store::room_cleanup::begin(&db, 31, Some("thread-old"), 2.0).unwrap();
                    cdr_store::room_cleanup::complete(&db, 31, &token).unwrap();
                    assert!(
                        sync.retire_exact_absent("thread-old", 31, 21)
                            .await
                            .is_err()
                    );
                    assert_eq!(
                        cdr_store::room_cleanup::phase(&db, 31).unwrap().as_deref(),
                        Some("deleted")
                    );
                } else {
                    assert!(sync.sync(99, None).await.is_err());
                    assert_eq!(cdr_store::room_cleanup::phase(&db, 31).unwrap(), None);
                }
                assert!(thread_channels(&db, "thread-old").unwrap().is_some());
                assert_eq!(remote.0.lock().unwrap().deletes, 0);
            }
        }
    }
}
#[tokio::test]
async fn custody_preserves_orphan_threads_and_project_containers() {
    for target in [21, 32] {
        for corrupt in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let (sync, remote) = fixture(&temp);
            let db = temp.path().join("mirror.sqlite");
            // Settle the obsolete thread first so this test reaches project/orphan cleanup.
            sync.sync(99, None).await.unwrap();
            if target == 21 {
                cdr_store::mapping::upsert_project(&db, "C:/obsolete", "old", 21, 0.0, |a, b| {
                    a == b
                })
                .unwrap();
                remote
                    .0
                    .lock()
                    .unwrap()
                    .channels
                    .insert(21, channel(21, None, ChannelType::GuildText, "old"));
            } else {
                remote.0.lock().unwrap().channels.insert(
                    32,
                    channel(32, Some(20), ChannelType::PublicThread, "orphan"),
                );
            }
            // Reconstruct a cold coordinator, matching Off/startup custody reading.
            drop(sync);
            protect(temp.path(), target, corrupt);
            let sync = MirrorSynchronizer::new(
                temp.path().join("state.sqlite"),
                db.clone(),
                remote.clone(),
                Some(1),
            );
            let before = remote.0.lock().unwrap().deletes;
            assert!(sync.sync(99, None).await.is_err());
            assert!(remote.0.lock().unwrap().channels.contains_key(&target));
            assert_eq!(remote.0.lock().unwrap().deletes, before);
            assert_eq!(
                cdr_store::room_cleanup::phase(&db, i64::try_from(target).unwrap()).unwrap(),
                None
            );
        }
    }
}
