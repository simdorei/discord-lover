//! Custody survives Off, missing CLI/configuration, and lifecycle failures.
use super::{AgyError, Result};
use crate::mirror_sync::{MirrorChannel, MirrorTransport};
use cdr_discord::gateway::topology::{GatewayTopology, TopologySnapshot};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, RwLock, Weak},
};
use tokio::sync::{Mutex, OwnedMutexGuard};

mod disk;
pub(super) use disk::{mark_configured, read_snapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Codex,
    Agy,
    Unmanaged,
    Uncertain,
    Conflict,
}
impl Owner {
    #[must_use]
    pub const fn permits_codex(self) -> bool {
        matches!(self, Self::Codex | Self::Unmanaged)
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct Snapshot {
    pub never_configured: bool,
    pub available: bool,
    pub pending_creations: HashSet<String>,
    pub revision: u64,
    pub agy: HashMap<u64, Option<u64>>,
    pub codex: HashSet<u64>,
    pub verified: HashMap<u64, (Option<u64>, u64)>,
}

pub struct AgyOwnership {
    pub(crate) root: PathBuf,
    guild: u64,
    snapshot: RwLock<Arc<Snapshot>>,
    topology: RwLock<GatewayTopology>,
    worker: OnceLock<std::result::Result<super::blocking::BlockingWorker, String>>,
    loaded: tokio::sync::OnceCell<std::result::Result<(), String>>,
    /// Local adoption/creation and cleanup reserve the same boundary.
    pub(super) coordination: Arc<Mutex<()>>,
}

pub struct CleanupPermit {
    ownership: Arc<AgyOwnership>,
    channel: u64,
    revision: u64,
    _reservation: OwnedMutexGuard<()>,
}
impl CleanupPermit {
    pub fn validate(&self) -> Result<()> {
        let current = self
            .ownership
            .snapshot
            .read()
            .map_err(|_| AgyError::Invalid("AGY ownership index unavailable".into()))?;
        // A changed version must be reclassified; unrelated topology must not
        // invalidate an otherwise identical local reservation.
        let _version_changed = current.revision != self.revision;
        if !current.available
            || !current.pending_creations.is_empty()
            || !self
                .ownership
                .classify_snapshot(&current, self.channel)
                .permits_codex()
        {
            return Err(AgyError::Invalid(
                "Ownership changed or is protected; cleanup deferred".into(),
            ));
        }
        Ok(())
    }
}

impl AgyOwnership {
    pub(super) fn new(root: PathBuf, guild: u64) -> Arc<Self> {
        Arc::new(Self {
            root,
            guild,
            snapshot: RwLock::new(Arc::new(Snapshot::default())),
            topology: RwLock::new(GatewayTopology::default()),
            coordination: Arc::new(Mutex::new(())),
            worker: OnceLock::new(),
            loaded: tokio::sync::OnceCell::new(),
        })
    }
    #[must_use]
    pub fn for_root(root: &Path, guild: u64) -> Arc<Self> {
        type Registry = std::sync::Mutex<HashMap<(PathBuf, u64), Weak<AgyOwnership>>>;
        static ROOTS: OnceLock<Registry> = OnceLock::new();
        let root = root
            .canonicalize()
            .or_else(|_| std::path::absolute(root))
            .unwrap_or_else(|_| root.to_owned());
        let key = (root.clone(), guild);
        let mut registry = ROOTS
            .get_or_init(Registry::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(owner) = registry.get(&key).and_then(Weak::upgrade) {
            return owner;
        }
        registry.retain(|_, value| value.strong_count() > 0);
        let owner = Self::new(root, guild);
        registry.insert(key, Arc::downgrade(&owner));
        owner
    }
    pub(super) fn worker(&self) -> Result<super::blocking::BlockingWorker> {
        self.worker
            .get_or_init(|| super::blocking::BlockingWorker::start().map_err(|e| e.to_string()))
            .clone()
            .map_err(AgyError::Invalid)
    }
    pub async fn ensure_loaded(&self, mirror_db: &Path) -> Result<()> {
        if self.snapshot.read().is_ok_and(|s| s.available) {
            return Ok(());
        }
        let outcome = self
            .loaded
            .get_or_init(|| async {
                let root = self.root.clone();
                let mirror = mirror_db.to_owned();
                let guild = self.guild;
                let result = async {
                    let state = self
                        .worker()?
                        .call_with_budget(super::blocking::OWNERSHIP_BUDGET, move || {
                            read_snapshot(&root, &mirror, guild)
                        })
                        .await?;
                    self.replace(state)
                }
                .await;
                result.map_err(|e: AgyError| e.to_string())
            })
            .await;
        outcome.clone().map_err(AgyError::Invalid)
    }
    pub(super) async fn refresh(&self, mirror_db: &Path) -> Result<()> {
        let root = self.root.clone();
        let mirror = mirror_db.to_owned();
        let guild = self.guild;
        match self
            .worker()?
            .call_with_budget(super::blocking::OWNERSHIP_BUDGET, move || {
                read_snapshot(&root, &mirror, guild)
            })
            .await
        {
            Ok(mut snapshot) => {
                let prior = self
                    .snapshot
                    .read()
                    .map_err(|_| AgyError::Invalid("Ownership lock poisoned".into()))?;
                for (id, parent) in &prior.agy {
                    snapshot.agy.entry(*id).or_insert(*parent);
                }
                snapshot
                    .pending_creations
                    .extend(prior.pending_creations.iter().cloned());
                snapshot.never_configured &= prior.agy.is_empty();
                snapshot.revision = prior.revision.saturating_add(1);
                drop(prior);
                self.replace(snapshot)
            }
            Err(error) => {
                self.unavailable();
                Err(error)
            }
        }
    }
    pub(super) fn replace(&self, snapshot: Snapshot) -> Result<()> {
        *self
            .snapshot
            .write()
            .map_err(|_| AgyError::Invalid("AGY ownership index unavailable".into()))? =
            Arc::new(snapshot);
        Ok(())
    }
    fn update(&self, change: impl FnOnce(&mut Snapshot)) -> Result<()> {
        let mut state = self
            .snapshot
            .write()
            .map_err(|_| AgyError::Invalid("AGY ownership index unavailable".into()))?;
        let state = Arc::make_mut(&mut state);
        change(state);
        state.revision = state.revision.saturating_add(1);
        Ok(())
    }
    pub(crate) fn attach_topology(&self, topology: GatewayTopology) -> Result<()> {
        *self
            .topology
            .write()
            .map_err(|_| AgyError::Invalid("AGY topology index unavailable".into()))? = topology;
        Ok(())
    }
    pub(super) fn topology_revision(&self) -> u64 {
        self.topology
            .read()
            .ok()
            .and_then(|t| t.read(|s| s.revision))
            .unwrap_or(u64::MAX)
    }
    pub(super) fn topology_epoch(&self) -> u64 {
        self.topology
            .read()
            .ok()
            .and_then(|t| t.read(|s| s.epoch))
            .unwrap_or(u64::MAX)
    }
    pub(super) fn unavailable(&self) {
        let _ = self.update(|s| {
            s.available = false;
            // Preserve evidence for already verified Codex execution in this
            // gateway epoch. Cleanup still requires healthy complete custody.
        });
    }
    pub(super) fn creation_pending(&self, receipt: &str) -> Result<()> {
        self.update(|s| {
            s.never_configured = false;
            s.pending_creations.insert(receipt.to_owned());
        })
    }
    pub(super) fn configured(&self) -> Result<()> {
        self.update(|s| s.never_configured = false)
    }
    /// Retain a confirmed receipt even when durable confirmation fails or On was cancelled.
    pub(super) fn remember(&self, id: u64, parent: Option<u64>) -> Result<()> {
        self.update(|s| {
            s.never_configured = false;
            s.agy.insert(id, parent);
        })
    }
    pub(super) fn creations_settled(&self, receipt: &str) -> Result<()> {
        self.update(|s| {
            s.pending_creations.remove(receipt);
        })
    }
    pub(super) fn known_ids(&self) -> Vec<u64> {
        self.snapshot
            .read()
            .map_or_else(|_| Vec::new(), |s| s.agy.keys().copied().collect())
    }

    #[must_use]
    pub fn classify(&self, id: u64) -> Owner {
        self.snapshot
            .read()
            .map_or(Owner::Uncertain, |s| self.classify_snapshot(&s, id))
    }
    fn classify_snapshot(&self, s: &Snapshot, id: u64) -> Owner {
        self.topology
            .read()
            .ok()
            .and_then(|t| t.read(|t| self.classify_topology(s, t, id)))
            .unwrap_or(Owner::Uncertain)
    }
    fn classify_topology(&self, s: &Snapshot, t: &TopologySnapshot, id: u64) -> Owner {
        let mut cursor = Some(id);
        let mut agy = false;
        let mut verified = false;
        for depth in 0..4 {
            let Some(current) = cursor else {
                verified = true;
                break;
            };
            if s.agy.contains_key(&current) {
                agy = true;
                break;
            }
            if let Some(channel) = t.channels.get(&current) {
                if channel.guild != Some(self.guild) {
                    return Owner::Uncertain;
                }
                cursor = channel.parent;
            } else if let Some((parent, _)) =
                s.verified.get(&current).filter(|(_, at)| *at == t.epoch)
            {
                cursor = *parent;
            } else {
                verified = false;
                break;
            }
            if depth == 3 {
                verified = false;
            }
        }
        if agy {
            return if s.codex.contains(&id) {
                Owner::Conflict
            } else {
                Owner::Agy
            };
        }
        if s.never_configured && s.available {
            return if s.codex.contains(&id) {
                Owner::Codex
            } else {
                Owner::Unmanaged
            };
        }
        if !verified || (!s.available && !s.codex.contains(&id)) {
            return Owner::Uncertain;
        }
        if s.codex.contains(&id) {
            Owner::Codex
        } else if !s.pending_creations.is_empty() {
            Owner::Uncertain
        } else {
            Owner::Unmanaged
        }
    }
    /// Called only by the bounded classifier/cleanup worker, never common ingress.
    pub(super) async fn verify(&self, id: u64, remote: &Arc<dyn MirrorTransport>) -> Result<Owner> {
        let owner = self.classify(id);
        if owner != Owner::Uncertain {
            return Ok(owner);
        }
        let revision = self.topology_revision();
        let epoch = self.topology_epoch();
        let mut current = Some(id);
        let mut observed = Vec::new();
        for _ in 0..4 {
            let Some(channel) = current else {
                break;
            };
            if self
                .snapshot
                .read()
                .is_ok_and(|s| s.agy.contains_key(&channel))
            {
                break;
            }
            let Some(item) = remote.channel(channel).await? else {
                // Absence is sufficient for a previously known mapping only when custody is healthy.
                observed.push((channel, None));
                break;
            };
            if item.id != channel || item.guild_id != Some(self.guild) {
                return Err(AgyError::Invalid(
                    "Discord ownership identity mismatch".into(),
                ));
            }
            current = item.parent_id;
            observed.push((item.id, item.parent_id));
        }
        if self.topology_revision() != revision {
            return Ok(Owner::Uncertain);
        }
        self.update(|s| {
            for (id, parent) in observed {
                s.verified.insert(id, (parent, epoch));
            }
        })?;
        Ok(self.classify(id))
    }
    pub(super) fn observe_remote(&self, channel: &MirrorChannel) -> Result<()> {
        if channel.guild_id != Some(self.guild) {
            return Err(AgyError::Invalid("Ownership guild mismatch".into()));
        }
        let epoch = self.topology_epoch();
        self.update(|s| {
            s.verified.insert(channel.id, (channel.parent_id, epoch));
        })
    }
    pub async fn cleanup(
        self: &Arc<Self>,
        channel: u64,
        remote: &Arc<dyn MirrorTransport>,
    ) -> Result<CleanupPermit> {
        let reservation = Arc::clone(&self.coordination).lock_owned().await;
        if !self.verify(channel, remote).await?.permits_codex() {
            return Err(AgyError::Invalid(
                "AGY/uncertain/conflicting ownership protects this room".into(),
            ));
        }
        let revision = self
            .snapshot
            .read()
            .map_err(|_| AgyError::Invalid("Ownership unavailable".into()))?
            .revision;
        let permit = CleanupPermit {
            ownership: Arc::clone(self),
            channel,
            revision,
            _reservation: reservation,
        };
        permit.validate()?;
        Ok(permit)
    }
}
