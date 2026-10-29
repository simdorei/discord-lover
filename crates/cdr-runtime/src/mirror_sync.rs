use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};
use thiserror::Error;
use tokio::sync::Mutex;
use twilight_model::channel::ChannelType;

mod archived_rejections;
mod channels;
mod cleanup;
mod container_creation;
mod creation;
mod deadline;
mod delete_guard;
mod exact_cleanup;
mod http;
mod inspection;
mod inspection_snapshot;
mod names;
mod new_thread;
mod orphan_cleanup;
mod rollout;
mod scope;
mod sync;
pub use http::DiscordMirrorTransport;

pub type MirrorFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, MirrorSyncError>> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq)]
pub struct MirrorChannel {
    pub id: u64,
    pub guild_id: Option<u64>,
    pub parent_id: Option<u64>,
    pub kind: ChannelType,
    pub name: String,
    pub topic: Option<String>,
    pub archived: bool,
}

#[derive(Clone, Debug)]
pub struct MirrorInventoryThread {
    pub channel: MirrorChannel,
    pub owned_by_bot: bool,
}

pub trait MirrorTransport: Send + Sync {
    fn thread_inventory(
        &self,
        guild: u64,
        parent: u64,
    ) -> MirrorFuture<'_, Vec<MirrorInventoryThread>>;
    fn delete(&self, id: u64) -> MirrorFuture<'_, ()>;
    fn channels(&self, guild: u64) -> MirrorFuture<'_, Vec<MirrorChannel>>;
    fn channel(&self, id: u64) -> MirrorFuture<'_, Option<MirrorChannel>>;
    fn create<'a>(
        &'a self,
        guild: u64,
        parent: Option<u64>,
        kind: ChannelType,
        name: &'a str,
        topic: Option<&'a str>,
    ) -> MirrorFuture<'a, MirrorChannel>;
    fn update<'a>(
        &'a self,
        channel: &'a MirrorChannel,
        name: &'a str,
        topic: Option<&'a str>,
        archived: bool,
    ) -> MirrorFuture<'a, ()>;
}

#[derive(Debug, Error)]
pub enum MirrorSyncError {
    #[error(
        "mirror sync stopped: room {channel} is protected by {reason}; no deletion was dispatched for this room; earlier sync changes may have completed"
    )]
    CleanupProtected { channel: u64, reason: &'static str },
    #[error("Discord rejected room deletion without deleting the room: {0}")]
    DeleteRejected(String),
    #[error(transparent)]
    State(#[from] cdr_codex_state::CodexStateError),
    #[error(transparent)]
    Store(#[from] cdr_store::StoreError),
    #[error("mirror sync Discord request failed: {0}")]
    Discord(String),
    #[error("mirror sync cannot continue: {0}")]
    Invalid(String),
    #[error(transparent)]
    Time(#[from] std::time::SystemTimeError),
}

pub struct MirrorSynchronizer {
    state_db: PathBuf,
    mirror_db: PathBuf,
    remote: Arc<dyn MirrorTransport>,
    guild_id: Option<u64>,
    lock: Mutex<()>,
    ownership: Arc<crate::agy::ownership::AgyOwnership>,
    #[cfg(test)]
    before_retirement: Option<Arc<dyn Fn(u64) + Send + Sync>>,
}

impl MirrorSynchronizer {
    #[must_use]
    pub fn new(
        state_db: PathBuf,
        mirror_db: PathBuf,
        remote: Arc<dyn MirrorTransport>,
        guild_id: Option<u64>,
    ) -> Self {
        let root = mirror_db
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let ownership = crate::agy::ownership::AgyOwnership::for_root(root, guild_id.unwrap_or(0));
        Self {
            state_db,
            mirror_db,
            remote,
            guild_id,
            lock: Mutex::new(()),
            ownership,
            #[cfg(test)]
            before_retirement: None,
        }
    }

    #[must_use]
    pub fn with_ownership(mut self, ownership: Arc<crate::agy::ownership::AgyOwnership>) -> Self {
        self.ownership = ownership;
        self
    }

    async fn custody(
        &self,
        channel: u64,
    ) -> Result<crate::agy::ownership::CleanupPermit, MirrorSyncError> {
        self.ownership
            .ensure_loaded(&self.mirror_db)
            .await
            .map_err(|_| MirrorSyncError::CleanupProtected {
                channel,
                reason: "unavailable AGY ownership",
            })?;
        self.ownership
            .cleanup(channel, &self.remote)
            .await
            .map_err(|_| MirrorSyncError::CleanupProtected {
                channel,
                reason: "AGY/uncertain/conflicting ownership",
            })
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct MirrorSyncResult {
    pub projects: usize,
    pub threads: usize,
    pub archived: usize,
    pub unavailable: usize,
    pub limited: bool,
    pub orphan_deleted: usize,
    pub projects_deleted: usize,
}

impl std::fmt::Display for MirrorSyncResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Mirror sync complete.\nprojects: {}\nthreads: {}\nstale_threads_deleted: {}\nunavailable: {}\norphan_threads_deleted: {}\nprojects_deleted: {}\ncleanup: {}",
            self.projects,
            self.threads,
            self.archived,
            self.unavailable,
            self.orphan_deleted,
            self.projects_deleted,
            if self.limited {
                "limited sync; existing mappings retained"
            } else {
                "obsolete mirror rooms deleted; current mappings protected"
            }
        )
    }
}

fn now() -> Result<f64, MirrorSyncError> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64())
}

fn db_id(id: u64) -> Result<i64, MirrorSyncError> {
    i64::try_from(id)
        .map_err(|_| MirrorSyncError::Invalid("Discord id exceeds database range".into()))
}

fn discord_id(id: i64) -> Result<u64, MirrorSyncError> {
    u64::try_from(id)
        .ok()
        .filter(|id| *id != 0)
        .ok_or_else(|| MirrorSyncError::Invalid("invalid stored Discord id".into()))
}
