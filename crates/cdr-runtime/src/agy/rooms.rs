use super::{
    AgyError, Result,
    config::AgyConfig,
    database::Database,
    ownership::{AgyOwnership, Owner},
};
use crate::mirror_sync::{MirrorChannel, MirrorTransport};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, RwLock},
};
use twilight_model::channel::ChannelType;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Room {
    Project(String),
    Session(u64),
    Unsupported,
}

pub(super) struct Rooms {
    pub projects: HashMap<String, u64>,
    known: RwLock<HashMap<u64, Room>>,
}

impl Rooms {
    #[cfg(test)]
    pub async fn prepare(
        config: &AgyConfig,
        store: &Database,
        remote: &Arc<dyn MirrorTransport>,
        guild: u64,
        mirror_db: &Path,
        ownership: &Arc<AgyOwnership>,
    ) -> Result<Self> {
        Self::prepare_cancellable(
            config,
            store,
            remote,
            guild,
            mirror_db,
            ownership,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .await
    }
    pub async fn prepare_cancellable(
        config: &AgyConfig,
        store: &Database,
        remote: &Arc<dyn MirrorTransport>,
        guild: u64,
        mirror_db: &Path,
        ownership: &Arc<AgyOwnership>,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Self> {
        let category = ensure_container(
            store,
            remote,
            guild,
            "category",
            None,
            ChannelType::GuildCategory,
            "AGY",
            None,
            mirror_db,
            config.category_id,
            ownership,
            cancel,
        )
        .await?;
        let mut known = HashMap::from([(category, Room::Unsupported)]);
        let mut projects = HashMap::new();
        for project in &config.projects {
            cancelled(cancel)?;
            store.bind_project(&project.name, &project.cwd).await?;
            let channel = ensure_container(
                store,
                remote,
                guild,
                &format!("project:{}", project.name),
                Some(category),
                ChannelType::GuildText,
                &format!("agy-{}", project.name),
                Some(&format!("AGY CLI project: {}", project.name)),
                mirror_db,
                project.channel_id,
                ownership,
                cancel,
            )
            .await?;
            projects.insert(project.name.clone(), channel);
            known.insert(channel, Room::Project(project.name.clone()));
        }
        for session in store.sessions().await? {
            unowned_async(store, mirror_db, session.channel).await?;
            // Removed configuration retains ownership but is not executable.
            known.insert(
                session.channel,
                if projects.contains_key(&session.project) {
                    Room::Session(session.channel)
                } else {
                    Room::Unsupported
                },
            );
        }
        Ok(Self {
            projects,
            known: RwLock::new(known),
        })
    }

    pub fn known(&self, id: u64) -> Result<Option<Room>> {
        Ok(self
            .known
            .read()
            .map_err(|_| AgyError::Invalid("AGY room index lock poisoned".into()))?
            .get(&id)
            .cloned())
    }

    pub fn remember_session(&self, id: u64) -> Result<()> {
        self.known
            .write()
            .map_err(|_| AgyError::Invalid("AGY room index lock poisoned".into()))?
            .insert(id, Room::Session(id));
        Ok(())
    }

    /// Classification precedes legacy room lookup, including stale Codex mappings.
    pub async fn resolve(
        &self,
        id: u64,
        _guild: u64,
        remote: &Arc<dyn MirrorTransport>,
        mirror_db: &Path,
        ownership: &Arc<AgyOwnership>,
        store: &Database,
    ) -> Result<Option<Room>> {
        let owner = ownership.verify(id, remote).await?;
        if owner.permits_codex() {
            return Ok(None);
        }
        if owner != Owner::Agy {
            return Err(AgyError::Invalid(
                "AGY room ownership is uncertain or conflicting".into(),
            ));
        }
        unowned_async(store, mirror_db, id).await?;
        Ok(Some(self.known(id)?.unwrap_or(Room::Unsupported)))
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "room identity is checked before persisting a creation receipt"
)]
async fn ensure_container(
    store: &Database,
    remote: &Arc<dyn MirrorTransport>,
    guild: u64,
    key: &str,
    parent: Option<u64>,
    kind: ChannelType,
    name: &str,
    topic: Option<&str>,
    mirror_db: &Path,
    configured_id: Option<u64>,
    ownership: &Arc<AgyOwnership>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<u64> {
    cancelled(cancel)?;
    let _custody = Arc::clone(&ownership.coordination).lock_owned().await;
    if let Some(saved) = store.container(key).await? {
        let id=saved.ok_or_else(||AgyError::Invalid(format!("AGY room creation outcome is unknown for {key}; inspect Discord before reconciling the saved receipt")))?;
        if configured_id.is_some_and(|requested| requested != id) {
            return Err(AgyError::Invalid(
                "Configured AGY channel differs from the saved channel; no mapping was changed"
                    .into(),
            ));
        }
        let channel = remote.channel(id).await?.ok_or_else(|| {
            AgyError::Invalid(format!(
                "Saved AGY room {id} is missing; no replacement was created"
            ))
        })?;
        validate(&channel, guild, parent, kind)?;
        unowned_async(store, mirror_db, id).await?;
        ownership.remember(id, parent)?;
        ownership.observe_remote(&channel)?;
        ownership.creations_settled(key)?;
        return Ok(id);
    }
    if let Some(id) = configured_id {
        let channel = remote
            .channel(id)
            .await?
            .ok_or_else(|| AgyError::Invalid("Configured AGY room is missing".into()))?;
        validate(&channel, guild, parent, kind)?;
        unowned_async(store, mirror_db, id).await?;
        ownership.creation_pending(key)?;
        store
            .begin_container(key, parent, &format!("{kind:?}"))
            .await?;
        ownership.remember(id, parent)?;
        store.confirm_container(key, id).await?;
        ownership.observe_remote(&channel)?;
        ownership.creations_settled(key)?;
        return Ok(id);
    }
    // Do not silently adopt or create a duplicate beside another bot's AGY room.
    if remote
        .channels(guild)
        .await?
        .iter()
        .any(|c| c.parent_id == parent && c.kind == kind && c.name == name)
    {
        return Err(AgyError::Invalid(format!(
            "An unregistered AGY room named {name} already exists; explicit migration is required"
        )));
    }
    cancelled(cancel)?;
    ownership.creation_pending(key)?;
    store
        .begin_container(key, parent, &format!("{kind:?}"))
        .await?;
    let channel = remote.create(guild, parent, kind, name, topic).await?;
    ownership.remember(channel.id, channel.parent_id)?;
    store.confirm_container(key, channel.id).await?;
    validate(&channel, guild, parent, kind)?;
    unowned_async(store, mirror_db, channel.id).await?;
    ownership.observe_remote(&channel)?;
    ownership.creations_settled(key)?;
    Ok(channel.id)
}

pub(super) async fn unowned_async(store: &Database, mirror_db: &Path, id: u64) -> Result<()> {
    let path = mirror_db.to_owned();
    store.worker.call(move || unowned(&path, id)).await
}

pub(super) fn validate(
    channel: &MirrorChannel,
    guild: u64,
    parent: Option<u64>,
    kind: ChannelType,
) -> Result<()> {
    if channel.id == 0
        || channel.guild_id != Some(guild)
        || channel.parent_id != parent
        || channel.kind != kind
    {
        return Err(AgyError::Invalid(
            "AGY channel identity/type/parent differs from its saved owner".into(),
        ));
    }
    Ok(())
}

pub(super) fn unowned(mirror_db: &Path, id: u64) -> Result<()> {
    if cdr_store::room_cleanup::phase(mirror_db, db_id(id)?)?.is_some() {
        return Err(AgyError::Invalid(
            "Discord room has a Codex cleanup fence; AGY adoption is blocked".into(),
        ));
    }
    if cdr_store::mapping::is_mirrored_channel(mirror_db, Some(db_id(id)?))? {
        return Err(AgyError::Invalid(format!(
            "Discord room {id} already belongs to the Codex mirror"
        )));
    }
    Ok(())
}

fn db_id(id: u64) -> Result<i64> {
    i64::try_from(id).map_err(|_| AgyError::Invalid("Discord id exceeds database range".into()))
}

fn cancelled(cancel: &std::sync::atomic::AtomicBool) -> Result<()> {
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        Err(AgyError::Stopped)
    } else {
        Ok(())
    }
}
