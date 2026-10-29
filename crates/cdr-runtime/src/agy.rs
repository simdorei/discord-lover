//! Optional AGY CLI adapter for the existing Discord gateway. AGY rooms and
//! conversation IDs never enter the Codex mirror or app-server stores.
use crate::{
    config::RuntimeConfig,
    mirror_sync::MirrorTransport,
    restart_readiness::drain::{AdmissionGate, AdmissionPermit},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use twilight_http::Client;

mod blocking;
pub mod config;
mod configuration;
#[cfg(windows)]
pub mod control;
mod database;
mod history;
mod ingress;
pub mod lifecycle;
pub mod manager;
pub mod ownership;
mod processes;
mod rooms;
pub(crate) mod routing;
mod runner;
pub mod store;
mod transport;
mod worker;

use config::AgyConfig;
use database::Database;
use ownership::AgyOwnership;
use rooms::{Room, Rooms};
#[cfg(test)]
use store::Store;
use transport::{DiscordMessenger, Messenger};

pub type Result<T> = std::result::Result<T, AgyError>;

#[derive(Debug, Error)]
pub enum AgyError {
    #[error("AGY configuration/state: {0}")]
    Invalid(String),
    #[error("AGY CLI: {0}")]
    Protocol(String),
    #[error("AGY Discord: {0}")]
    Discord(String),
    #[error("AGY 작업을 중단했습니다.")]
    Stopped,
    #[error("AGY 소유 프로세스 종료가 확인되지 않았습니다. 재활성화를 보류합니다.")]
    CleanupUnconfirmed,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Mirror(#[from] crate::mirror_sync::MirrorSyncError),
    #[error(transparent)]
    Store(#[from] cdr_store::StoreError),
    #[error(transparent)]
    Drain(#[from] crate::restart_readiness::drain::DrainGateError),
    #[cfg(windows)]
    #[error(transparent)]
    Native(#[from] cdr_windows_native::NativeError),
}

pub struct AgyRuntime {
    config: AgyConfig,
    policy: Arc<RuntimeConfig>,
    guild: u64,
    mirror_db: PathBuf,
    store: Database,
    ownership: Arc<AgyOwnership>,
    processes: processes::Processes,
    handlers: std::sync::atomic::AtomicUsize,
    acceptance: tokio::sync::Mutex<Acceptance>,
    rooms: Rooms,
    remote: Arc<dyn MirrorTransport>,
    messenger: Arc<dyn Messenger>,
    history: Arc<dyn history::History>,
    admission: AdmissionGate,
    sender: mpsc::Sender<Job>,
    active: Arc<Mutex<HashMap<String, Active>>>,
    #[cfg(test)]
    submit_barrier: Option<Arc<worker::SubmitBarrier>>,
}

pub(crate) struct PreparedAgy {
    pub service: Arc<AgyRuntime>,
    receiver: mpsc::Receiver<Job>,
}

struct Active {
    origin: u64,
    target: Option<u64>,
    stop: watch::Sender<bool>,
}
struct Job {
    id: String,
    origin: u64,
    room: Room,
    prompt: String,
    new: bool,
    stop: watch::Receiver<bool>,
    _permit: AdmissionPermit,
    active: Arc<Mutex<HashMap<String, Active>>>,
}

struct Acceptance {
    open: bool,
    generation: u64,
    cutoff: u64,
}
impl Drop for Job {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(&self.id);
        }
    }
}

impl AgyRuntime {
    #[allow(
        clippy::too_many_arguments,
        reason = "explicit activation dependencies keep transport and cancellation testable"
    )]
    pub(crate) async fn prepare(
        config: AgyConfig,
        policy: Arc<RuntimeConfig>,
        root: &Path,
        mirror_db: PathBuf,
        http: Arc<Client>,
        admission: AdmissionGate,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        remote: Arc<dyn MirrorTransport>,
    ) -> Result<PreparedAgy> {
        let guild = policy
            .guild_id
            .ok_or_else(|| AgyError::Invalid("DISCORD_GUILD_ID is required for AGY".into()))?;
        let ownership = AgyOwnership::for_root(root, guild);
        ownership.ensure_loaded(&mirror_db).await?;
        let worker = ownership.worker()?;
        ownership.configured()?;
        let root_owned = root.to_owned();
        worker
            .call(move || ownership::mark_configured(&root_owned, guild))
            .await?;
        let store = Database::open(worker, root.join(".codex_discord_agy.sqlite"), guild).await?;
        let rooms = Rooms::prepare_cancellable(
            &config, &store, &remote, guild, &mirror_db, &ownership, &cancel,
        )
        .await?;
        let messenger = Arc::new(DiscordMessenger(Arc::clone(&http)));
        let (sender, receiver) = mpsc::channel(8);
        let service = Arc::new(Self {
            config,
            policy,
            guild,
            mirror_db,
            store,
            ownership,
            processes: processes::Processes::default(),
            handlers: std::sync::atomic::AtomicUsize::new(0),
            acceptance: tokio::sync::Mutex::new(Acceptance {
                open: false,
                generation: 0,
                cutoff: u64::MAX,
            }),
            rooms,
            remote,
            messenger,
            history: Arc::new(history::DiscordHistory(Arc::clone(&http))),
            admission,
            sender,
            active: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            submit_barrier: None,
        });
        Ok(PreparedAgy { service, receiver })
    }

    fn active(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Active>>> {
        self.active
            .lock()
            .map_err(|_| AgyError::Invalid("AGY execution lock poisoned".into()))
    }

    pub(super) async fn close_admission(&self) {
        self.acceptance.lock().await.open = false;
    }
    pub(super) fn quiescent(&self) -> bool {
        self.active().is_ok_and(|a| a.is_empty())
            && self.processes.count() == 0
            && self.store.worker.idle()
            && self.handlers.load(std::sync::atomic::Ordering::Acquire) == 0
    }
    pub(super) fn stop_all(&self) -> Result<()> {
        for active in self.active()?.values() {
            let _ = active.stop.send(true);
        }
        Ok(())
    }
    pub(super) async fn open_admission(
        &self,
        generation: u64,
        epoch: String,
        cutoff: u64,
    ) -> Result<()> {
        let mut gate = self.acceptance.lock().await;
        let channels = self.ownership.known_ids();
        self.store
            .call(move |store| store.activate(generation, &epoch, cutoff, &channels))
            .await?;
        *gate = Acceptance {
            open: true,
            generation,
            cutoff,
        };
        Ok(())
    }

    fn user_allowed(&self, user: Option<u64>) -> bool {
        user.is_some_and(|id| {
            self.policy.allowed_user_ids.is_empty() || self.policy.allowed_user_ids.contains(&id)
        })
    }
}

impl PreparedAgy {
    pub(crate) async fn run(self, shutdown: watch::Receiver<bool>) -> Result<()> {
        worker::run(self.service, self.receiver, shutdown).await
    }
}

#[cfg(test)]
mod tests;
