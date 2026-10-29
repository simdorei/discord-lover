use cdr_runtime::mirror_sync::{
    MirrorChannel, MirrorFuture, MirrorInventoryThread, MirrorSyncError, MirrorSynchronizer,
    MirrorTransport,
};
use cdr_store::mapping::{mirror_targets, thread_channels, upsert_project, upsert_thread};
use rusqlite::Connection;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    sync::{Arc, Mutex},
};
use twilight_model::channel::ChannelType;

#[path = "support/action_target.rs"]
mod action_target;
#[path = "support/mirror_sync_cleanup_cases.rs"]
mod cleanup_cases;
#[path = "support/mirror_cleanup_races.rs"]
mod cleanup_races;
#[path = "support/mirror_container_creation.rs"]
mod container_creation;
#[path = "support/mirror_creation_outcome.rs"]
mod creation_outcome;
#[path = "support/mirror_deadline.rs"]
mod deadline;
#[path = "support/mirror_exact_cleanup.rs"]
mod exact_cleanup;
#[path = "support/mirror_missing_cleanup.rs"]
mod missing_cleanup;
#[path = "support/mirror_owned_cleanup.rs"]
mod owned_cleanup;
#[path = "support/mirror_rollout_delivery.rs"]
mod rollout_delivery;
#[path = "support/mirror_rollout_recovery.rs"]
mod rollout_recovery;

#[derive(Default)]
struct RemoteState {
    channels: BTreeMap<u64, MirrorChannel>,
    creates: usize,
    deletes: usize,
    lose_create_response: Option<ChannelType>,
    pause_create: Option<(
        ChannelType,
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
    )>,
    fail_id: Option<u64>,
    foreign_ids: BTreeSet<u64>,
    fail_delete: Option<u64>,
    before_delete: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    lose_delete_response: Option<u64>,
    pause_delete: Option<(u64, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    pause_delete_after: Option<(u64, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    pause_channel: Option<(u64, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
}
#[derive(Default)]
struct Remote(Mutex<RemoteState>);
impl MirrorTransport for Remote {
    fn thread_inventory(
        &self,
        _: u64,
        parent: u64,
    ) -> MirrorFuture<'_, Vec<MirrorInventoryThread>> {
        Box::pin(async move {
            let state = self.0.lock().unwrap();
            Ok(state
                .channels
                .values()
                .filter(|c| c.kind.is_thread() && c.parent_id == Some(parent))
                .map(|c| MirrorInventoryThread {
                    channel: c.clone(),
                    owned_by_bot: !state.foreign_ids.contains(&c.id),
                })
                .collect())
        })
    }
    fn delete(&self, id: u64) -> MirrorFuture<'_, ()> {
        Box::pin(async move {
            self.0.lock().unwrap().deletes += 1;
            let pause = self.0.lock().unwrap().pause_delete.clone();
            if let Some((paused_id, started, release)) = pause
                && paused_id == id
            {
                started.notify_one();
                release.notified().await;
            }
            let (lost, after) = {
                let mut state = self.0.lock().unwrap();
                if state.fail_delete == Some(id) {
                    return Err(MirrorSyncError::DeleteRejected(
                        "HTTP 403: Missing Access".into(),
                    ));
                }
                if let Some(callback) = &state.before_delete {
                    callback(id);
                }
                state.channels.remove(&id);
                (
                    state.lose_delete_response == Some(id),
                    state.pause_delete_after.clone(),
                )
            };
            if let Some((paused_id, started, release)) = after
                && paused_id == id
            {
                started.notify_one();
                release.notified().await;
            }
            if lost {
                return Err(MirrorSyncError::Discord("delete response lost".into()));
            }
            Ok(())
        })
    }
    fn channels(&self, _: u64) -> MirrorFuture<'_, Vec<MirrorChannel>> {
        Box::pin(async {
            Ok(self
                .0
                .lock()
                .unwrap()
                .channels
                .values()
                .filter(|c| !c.kind.is_thread())
                .cloned()
                .collect())
        })
    }
    fn channel(&self, id: u64) -> MirrorFuture<'_, Option<MirrorChannel>> {
        Box::pin(async move {
            let pause = self.0.lock().unwrap().pause_channel.clone();
            if let Some((paused_id, started, release)) = pause
                && paused_id == id
            {
                started.notify_one();
                release.notified().await;
            }
            let state = self.0.lock().unwrap();
            if state.fail_id == Some(id) {
                return Err(MirrorSyncError::Discord("HTTP 403: Missing Access".into()));
            }
            Ok(state.channels.get(&id).cloned())
        })
    }
    fn create<'a>(
        &'a self,
        guild: u64,
        parent: Option<u64>,
        kind: ChannelType,
        name: &'a str,
        topic: Option<&'a str>,
    ) -> MirrorFuture<'a, MirrorChannel> {
        Box::pin(async move {
            let (channel, lost, pause) = {
                let mut state = self.0.lock().unwrap();
                state.creates += 1;
                let channel = MirrorChannel {
                    id: 1000 + state.creates as u64,
                    guild_id: Some(guild),
                    parent_id: parent,
                    kind,
                    name: name.into(),
                    topic: topic.map(str::to_owned),
                    archived: false,
                };
                state.channels.insert(channel.id, channel.clone());
                (
                    channel,
                    state.lose_create_response == Some(kind),
                    state.pause_create.clone(),
                )
            };
            if let Some((paused_kind, started, release)) = pause
                && paused_kind == kind
            {
                started.notify_one();
                release.notified().await;
            }
            if lost {
                return Err(MirrorSyncError::Discord("create response lost".into()));
            }
            Ok(channel)
        })
    }
    fn update<'a>(
        &'a self,
        channel: &'a MirrorChannel,
        name: &'a str,
        topic: Option<&'a str>,
        archived: bool,
    ) -> MirrorFuture<'a, ()> {
        Box::pin(async move {
            let mut state = self.0.lock().unwrap();
            let current = state.channels.get_mut(&channel.id).unwrap();
            current.name = name.into();
            if let Some(topic) = topic {
                current.topic = Some(topic.into());
            }
            current.archived = archived;
            Ok(())
        })
    }
}

fn channel(id: u64, parent: Option<u64>, kind: ChannelType, name: &str) -> MirrorChannel {
    MirrorChannel {
        id,
        guild_id: Some(1),
        parent_id: parent,
        kind,
        name: name.into(),
        topic: None,
        archived: false,
    }
}

fn fixture(temp: &tempfile::TempDir) -> (MirrorSynchronizer, Arc<Remote>) {
    let state = temp.path().join("state.sqlite");
    let mirror = temp.path().join("mirror.sqlite");
    let db = Connection::open(&state).unwrap();
    db.execute_batch(include_str!("fixtures/action_state.sql"))
        .unwrap();
    let rollout = temp.path().join("rollout.jsonl");
    fs::write(&rollout, "").unwrap();
    db.execute(
        "UPDATE threads SET rollout_path = ?",
        [rollout.to_string_lossy().as_ref()],
    )
    .unwrap();
    fs::write(
        temp.path().join("session_index.jsonl"),
        "{\"id\":\"thread-a\",\"thread_name\":\"앱에서 정한 이름\"}\n",
    )
    .unwrap();
    upsert_project(&mirror, "C:/repos/alpha", "alpha", 20, 1.0, |a, b| a == b).unwrap();
    upsert_thread(
        &mirror,
        "thread-a",
        "C:/repos/alpha",
        "old title",
        20,
        30,
        1.0,
    )
    .unwrap();
    upsert_thread(&mirror, "thread-old", "C:/repos/old", "old", 21, 31, 1.0).unwrap();
    let remote = Arc::new(Remote::default());
    remote.0.lock().unwrap().channels = [
        channel(10, None, ChannelType::GuildCategory, "Codex"),
        channel(20, Some(10), ChannelType::GuildText, "codex-alpha"),
        channel(21, Some(10), ChannelType::GuildText, "codex-old"),
        channel(30, Some(20), ChannelType::PublicThread, "old title"),
        channel(31, Some(21), ChannelType::PublicThread, "old"),
    ]
    .into_iter()
    .map(|c| (c.id, c))
    .collect();
    (
        MirrorSynchronizer::new(state, mirror, remote.clone(), Some(1)),
        remote,
    )
}

#[tokio::test]
async fn full_codex_sync_preserves_same_bot_agy_category_and_native_conversation() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let agy_path = temp.path().join("agy.sqlite");
    let agy = cdr_runtime::agy::store::Store::open(&agy_path, 1).unwrap();
    agy.create_session(82, "alpha", temp.path()).unwrap();
    agy.save_conversation(82, "agy-native-conversation")
        .unwrap();
    {
        let mut state = remote.0.lock().unwrap();
        // The same bot owns both active and archived AGY threads. Even a project
        // name matching Codex must stay outside the Codex category's ownership.
        for room in [
            channel(80, None, ChannelType::GuildCategory, "AGY"),
            channel(81, Some(80), ChannelType::GuildText, "codex-alpha"),
            channel(82, Some(81), ChannelType::PublicThread, "AGY active"),
            channel(83, Some(81), ChannelType::PublicThread, "AGY archived"),
        ] {
            state.channels.insert(room.id, room);
        }
        state.channels.get_mut(&83).unwrap().archived = true;
    }
    sync.sync(99, None).await.unwrap();
    sync.sync(99, None).await.unwrap();
    let state = remote.0.lock().unwrap();
    assert!(
        !state.channels.contains_key(&31),
        "Codex's own stale cleanup must still run"
    );
    for id in [80, 81, 82, 83] {
        assert!(state.channels.contains_key(&id));
    }
    assert!(state.channels[&83].archived);
    drop(state);
    drop(agy);
    let reopened = cdr_runtime::agy::store::Store::open(&agy_path, 1).unwrap();
    assert_eq!(
        reopened
            .session(82)
            .unwrap()
            .unwrap()
            .conversation
            .as_deref(),
        Some("agy-native-conversation")
    );
    assert!(
        !cdr_store::mapping::is_mirrored_channel(&temp.path().join("mirror.sqlite"), Some(81))
            .unwrap()
    );
}

#[tokio::test]
async fn full_sync_creates_renames_and_deletes_obsolete_discord_threads() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let result = sync.sync(99, None).await.unwrap();
    assert_eq!(
        (result.projects, result.threads, result.archived),
        (2, 2, 1)
    );
    assert_eq!(
        mirror_targets(&temp.path().join("mirror.sqlite"), 100)
            .unwrap()
            .len(),
        2
    );
    assert!(
        thread_channels(&temp.path().join("mirror.sqlite"), "thread-b")
            .unwrap()
            .is_some()
    );
    let creates = {
        let state = remote.0.lock().unwrap();
        assert_eq!(state.channels[&30].name, "앱에서 정한 이름");
        assert!(
            !state.channels.contains_key(&31),
            "approved stale room must be deleted, not merely archived"
        );
        assert_eq!(
            state.channels[&20].topic.as_deref(),
            Some("Codex project mirror: alpha")
        );
        state.creates
    };
    sync.sync(99, None).await.unwrap();
    assert_eq!(
        remote.0.lock().unwrap().creates,
        creates,
        "sync retry must reuse mappings"
    );
}

#[tokio::test]
async fn sync_preserves_source_and_child_as_two_independent_conversations() {
    use cdr_store::queue::{
        NewAppServerForkHandoff, begin_app_server_fork_handoff, complete_app_server_fork_handoff,
    };
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let db = temp.path().join("mirror.sqlite");
    begin_app_server_fork_handoff(
        &db,
        NewAppServerForkHandoff {
            handoff_id: "legacy",
            ambiguous_job_id: None,
            source_thread_id: "thread-a",
            expected_generation: 1,
            quarantine_reason: "legacy copy",
        },
    )
    .unwrap();
    complete_app_server_fork_handoff(&db, "legacy", "thread-b", 1).unwrap();
    upsert_thread(&db, "thread-a", "C:/repos/alpha", "Original", 20, 32, 1.0).unwrap();
    remote.0.lock().unwrap().channels.insert(
        32,
        channel(32, Some(20), ChannelType::PublicThread, "Original"),
    );
    Connection::open(temp.path().join("state.sqlite"))
        .unwrap()
        .execute(
            "UPDATE threads SET cwd='C:/repos/alpha' WHERE id='thread-b'",
            [],
        )
        .unwrap();
    let result = sync.sync(99, Some(100)).await.unwrap();
    assert_eq!(result.threads, 2);
    assert_eq!(thread_channels(&db, "thread-a").unwrap(), Some((20, 32)));
    assert_eq!(thread_channels(&db, "thread-b").unwrap(), Some((20, 30)));
}

#[tokio::test]
async fn full_sync_removes_a_reopened_orphan_after_its_mapping_was_retired() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let orphan = channel(32, Some(20), ChannelType::PublicThread, "reopened old room");
    remote.0.lock().unwrap().channels.insert(32, orphan);
    sync.sync(99, None).await.unwrap();
    assert!(!remote.0.lock().unwrap().channels.contains_key(&32));
}

#[tokio::test]
async fn limited_sync_does_not_retire_other_threads() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    let result = sync.sync(99, Some(1)).await.unwrap();
    assert_eq!((result.threads, result.archived), (1, 0));
    assert!(
        thread_channels(&temp.path().join("mirror.sqlite"), "thread-old")
            .unwrap()
            .is_some()
    );
    assert!(!remote.0.lock().unwrap().channels[&31].archived);
}

#[tokio::test]
async fn inaccessible_stored_channel_surfaces_the_error_and_does_not_create_a_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    remote.0.lock().unwrap().fail_id = Some(30);
    let error = sync.sync(99, Some(1)).await.unwrap_err();
    assert!(error.to_string().contains("HTTP 403: Missing Access"));
    assert_eq!(remote.0.lock().unwrap().creates, 0);
    assert_eq!(
        thread_channels(&temp.path().join("mirror.sqlite"), "thread-a").unwrap(),
        Some((20, 30))
    );
}

#[tokio::test]
async fn missing_stored_thread_is_recreated_and_remapped() {
    let temp = tempfile::tempdir().unwrap();
    let (sync, remote) = fixture(&temp);
    remote.0.lock().unwrap().channels.remove(&30);
    sync.sync(99, Some(1)).await.unwrap();
    let mapping = thread_channels(&temp.path().join("mirror.sqlite"), "thread-a")
        .unwrap()
        .unwrap();
    assert_eq!(mapping.0, 20);
    assert_ne!(mapping.1, 30);
}

#[path = "support/mirror_archive_rejection.rs"]
mod archive_rejection;

#[path = "support/mirror_agy_custody.rs"]
mod agy_custody;
