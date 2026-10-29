use super::super::*;
use crate::mirror_sync::{MirrorChannel, MirrorFuture, MirrorInventoryThread, MirrorSyncError};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
};
use twilight_model::{
    channel::{ChannelType, Message},
    id::Id,
};

#[derive(Default)]
pub(in crate::agy) struct Remote {
    pub channels: Mutex<HashMap<u64, MirrorChannel>>,
    pub reads: AtomicUsize,
    pub codex_reads: AtomicUsize,
    pub behavior: AtomicUsize,
    pub create_barrier: Mutex<Option<Arc<RemoteBarrier>>>,
    pub panic_barrier: Mutex<Option<Arc<RemoteBarrier>>>,
    pub creates: AtomicUsize,
}
#[derive(Default)]
pub(in crate::agy) struct RemoteBarrier {
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}
impl MirrorTransport for Remote {
    fn channel(&self, id: u64) -> MirrorFuture<'_, Option<MirrorChannel>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if id == 90 {
            self.codex_reads.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async move {
            let barrier = self.panic_barrier.lock().unwrap().clone();
            if let Some(barrier) = barrier {
                barrier.entered.notify_one();
                barrier.release.notified().await;
                panic!("fixture channel failure");
            }
            match self.behavior.load(Ordering::SeqCst) {
                1 => return Err(MirrorSyncError::Invalid("fixture lookup failure".into())),
                2 => panic!("fixture lookup panic"),
                3 => tokio::time::sleep(std::time::Duration::from_secs(5)).await,
                _ => {}
            }
            Ok(self.channels.lock().unwrap().get(&id).cloned())
        })
    }
    fn channels(&self, _guild: u64) -> MirrorFuture<'_, Vec<MirrorChannel>> {
        Box::pin(async move { Ok(self.channels.lock().unwrap().values().cloned().collect()) })
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
            self.creates.fetch_add(1, Ordering::SeqCst);
            let barrier = self.create_barrier.lock().unwrap().clone();
            if let Some(barrier) = barrier {
                barrier.entered.notify_one();
                barrier.release.notified().await;
            }
            let mut channels = self.channels.lock().unwrap();
            let id = channels.keys().max().copied().unwrap_or(100) + 1;
            let channel = MirrorChannel {
                id,
                guild_id: Some(guild),
                parent_id: parent,
                kind,
                name: name.into(),
                topic: topic.map(str::to_owned),
                archived: false,
            };
            channels.insert(id, channel.clone());
            Ok(channel)
        })
    }
    fn update<'a>(
        &'a self,
        _channel: &'a MirrorChannel,
        _name: &'a str,
        _topic: Option<&'a str>,
        _archived: bool,
    ) -> MirrorFuture<'a, ()> {
        Box::pin(async { Err(MirrorSyncError::Invalid("unexpected update".into())) })
    }
    fn delete(&self, _id: u64) -> MirrorFuture<'_, ()> {
        panic!("AGY must not delete any room")
    }
    fn thread_inventory(
        &self,
        _guild: u64,
        _parent: u64,
    ) -> MirrorFuture<'_, Vec<MirrorInventoryThread>> {
        panic!("AGY must not scan all conversation history")
    }
}

#[derive(Default)]
pub(in crate::agy) struct Messages(pub Mutex<Vec<(u64, String)>>);
impl Messenger for Messages {
    fn send<'a>(&'a self, channel: u64, _key: &'a str, text: &'a str) -> transport::SendFuture<'a> {
        self.0.lock().unwrap().push((channel, text.into()));
        Box::pin(async { Ok(()) })
    }
}

pub(in crate::agy) struct Fixture {
    pub temp: tempfile::TempDir,
    pub service: Arc<AgyRuntime>,
    pub receiver: mpsc::Receiver<Job>,
    pub remote: Arc<Remote>,
    pub messages: Arc<Messages>,
    pub project: u64,
    pub thread: u64,
}

#[allow(
    clippy::too_many_lines,
    reason = "one cohesive lifecycle or contract scenario preserves ordering visibly"
)]
pub(in crate::agy) async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let config = AgyConfig {
        executable: std::env::current_exe().unwrap(),
        projects: vec![config::Project {
            name: "test".into(),
            cwd: temp.path().into(),
            channel_id: None,
        }],
        category_id: None,
        model: None,
        auto_approve: false,
    };
    let policy = Arc::new(
        RuntimeConfig::from_map(
            &BTreeMap::from([
                ("DISCORD_BOT_TOKEN".into(), "local-test".into()),
                ("DISCORD_ALLOW_ALL_CHANNELS".into(), "1".into()),
                ("DISCORD_GUILD_ID".into(), "1".into()),
                ("DISCORD_ALLOWED_USER_IDS".into(), "7".into()),
            ]),
            crate::config::CliOptions::default(),
        )
        .unwrap(),
    );
    let mirror_db = temp.path().join("mirror.sqlite");
    cdr_store::mapping::upsert_project(&mirror_db, "C:/codex", "codex", 90, 1.0, |a, b| a == b)
        .unwrap();
    let remote = Arc::new(Remote::default());
    let transport: Arc<dyn MirrorTransport> = remote.clone();
    let ownership = AgyOwnership::for_root(temp.path(), 1);
    ownership.ensure_loaded(&mirror_db).await.unwrap();
    let store = Database::open(
        ownership.worker().unwrap(),
        temp.path().join(".codex_discord_agy.sqlite"),
        1,
    )
    .await
    .unwrap();
    ownership
        .observe_remote(&MirrorChannel {
            id: 90,
            guild_id: Some(1),
            parent_id: None,
            kind: ChannelType::GuildText,
            name: "codex".into(),
            topic: None,
            archived: false,
        })
        .unwrap();
    let rooms = Rooms::prepare(&config, &store, &transport, 1, &mirror_db, &ownership)
        .await
        .unwrap();
    let project = rooms.projects["test"];
    let thread = 200;
    remote.channels.lock().unwrap().insert(
        thread,
        MirrorChannel {
            id: thread,
            guild_id: Some(1),
            parent_id: Some(project),
            kind: ChannelType::PublicThread,
            name: "conversation".into(),
            topic: None,
            archived: false,
        },
    );
    store
        .create_session(thread, "test", temp.path())
        .await
        .unwrap();
    store
        .save_conversation(thread, "native-saved-id")
        .await
        .unwrap();
    ownership.remember(thread, Some(project)).unwrap();
    rooms.remember_session(thread).unwrap();
    let messages = Arc::new(Messages::default());
    let (sender, receiver) = mpsc::channel(8);
    let service = Arc::new(AgyRuntime {
        config,
        policy,
        guild: 1,
        mirror_db,
        store,
        ownership,
        processes: processes::Processes::default(),
        handlers: std::sync::atomic::AtomicUsize::new(0),
        acceptance: tokio::sync::Mutex::new(Acceptance {
            open: true,
            generation: 1,
            cutoff: 0,
        }),
        rooms,
        remote: transport,
        messenger: messages.clone(),
        history: Arc::new(FailedHistory),
        admission: AdmissionGate::new(),
        sender,
        active: Arc::new(Mutex::new(HashMap::new())),
        submit_barrier: None,
    });
    Fixture {
        temp,
        service,
        receiver,
        remote,
        messages,
        project,
        thread,
    }
}

pub(in crate::agy) struct FailedHistory;
impl history::History for FailedHistory {
    fn recent(&self, _: u64) -> history::HistoryFuture<'_> {
        Box::pin(async { Err(AgyError::Discord("fixture history failure".into())) })
    }
}

pub(in crate::agy) fn message(id: u64, channel: u64, content: &str) -> Message {
    serde_json::from_value(serde_json::json!({
        "attachments":[],"author":{"avatar":null,"bot":false,"discriminator":"0001","id":"7","username":"tester"},
        "channel_id":channel.to_string(),"guild_id":"1","content":content,"edited_timestamp":null,"embeds":[],"id":id.to_string(),
        "mention_everyone":false,"mention_roles":[],"mentions":[],"pinned":false,"timestamp":"2026-10-28T00:00:00.000000+00:00","tts":false,"type":0
    })).unwrap()
}

pub(super) fn other_user(message: &mut Message) {
    message.author.id = Id::new(8);
}
