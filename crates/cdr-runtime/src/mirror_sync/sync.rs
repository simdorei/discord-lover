use super::{MirrorSyncError, MirrorSyncResult, MirrorSynchronizer, db_id, names, now};
use cdr_codex_state::load_session_thread_names;
use cdr_store::mapping::{MirrorThreadUpdate, commit_thread_sync};
use std::collections::BTreeMap;

impl MirrorSynchronizer {
    pub async fn sync_with_origin(
        &self,
        channel: u64,
        limit: Option<i64>,
        origin: serde_json::Value,
    ) -> Result<MirrorSyncResult, MirrorSyncError> {
        super::SYNC_ORIGIN
            .scope(origin, self.sync(channel, limit))
            .await
    }
    pub async fn sync(
        &self,
        origin_channel: u64,
        limit: Option<i64>,
    ) -> Result<MirrorSyncResult, MirrorSyncError> {
        self.with_operation_deadline("sync", false, self.sync_locked(origin_channel, limit))
            .await
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Keep the ordered validation and effect boundary visible in one workflow"
    )]
    async fn sync_locked(
        &self,
        origin_channel: u64,
        limit: Option<i64>,
    ) -> Result<MirrorSyncResult, MirrorSyncError> {
        let started = now()?;
        // Validate local read positions before changing Discord rooms. Missing
        // plain logs may be restored here; no saved cursor is ever reset by sync.
        let threads = self
            .scope(limit)?
            .into_iter()
            .map(|thread| {
                let available = super::rollout::ensure(&self.mirror_db, &thread)?;
                Ok((thread, available))
            })
            .collect::<Result<Vec<_>, MirrorSyncError>>()?;
        let guild = match self.guild_id {
            Some(guild) => guild,
            None => self
                .remote
                .channel(origin_channel)
                .await?
                .and_then(|channel| channel.guild_id)
                .ok_or_else(|| {
                    MirrorSyncError::Invalid(
                        "cannot resolve the Discord guild for this command".into(),
                    )
                })?,
        };
        let mut channels = self.remote.channels(guild).await?;
        let category = self.category_channel(guild, &channels).await?;
        let index = self
            .state_db
            .parent()
            .ok_or_else(|| MirrorSyncError::Invalid("Codex state has no directory".into()))?
            .join("session_index.jsonl");
        let titles = load_session_thread_names(&index)?;
        let mut projects = BTreeMap::new();
        let mut result = MirrorSyncResult {
            limited: limit.is_some(),
            ..MirrorSyncResult::default()
        };
        if limit.is_none() {
            self.reconcile_retirements(guild, &mut result).await?;
        }
        for (thread, available) in threads {
            if cdr_store::archived_cleanup::unresolved_target(&self.mirror_db, &thread.id)? {
                result.skipped.push(format!(
                    "thread {}: prior cleanup unresolved; create/reuse/update excluded",
                    thread.id
                ));
                continue;
            }
            if let Some((parent, room)) =
                cdr_store::mapping::thread_channels(&self.mirror_db, &thread.id)?
                && (self.room_excluded(room)? || self.room_excluded(parent)?)
            {
                result.skipped.push(format!(
                    "room {room}: prior cleanup unresolved; mapping/create/reuse excluded"
                ));
                continue;
            }
            if !available {
                result.unavailable += 1;
                eprintln!(
                    "mirror_sync_unavailable thread={} reason=rollout_missing",
                    thread.id
                );
                continue;
            }
            let (key, project_name) = names::project(&thread);
            if let Some(stored) = cdr_store::mapping::find_project(
                &self.mirror_db,
                Some(&key),
                super::channels::keys_match,
            )? && self.room_excluded(stored.channel_id)?
            {
                result.skipped.push(format!(
                    "project {project_name}: prior cleanup unresolved; create/reuse/update excluded"
                ));
                continue;
            }
            let parent = if let Some(parent) = projects.get(&key) {
                *parent
            } else {
                let channel = self
                    .project_channel(guild, category, &key, &project_name, &mut channels)
                    .await?;
                projects.insert(key.clone(), channel.id);
                channel.id
            };
            let title =
                names::thread_name(titles.get(&thread.id).unwrap_or(&thread.title), &thread.id);
            let (channel, expected) = self
                .thread_channel(guild, parent, &thread.id, &title)
                .await?;
            commit_thread_sync(
                &self.mirror_db,
                MirrorThreadUpdate {
                    thread_id: &thread.id,
                    project_key: &key,
                    title: &title,
                    parent_id: db_id(parent)?,
                    channel_id: db_id(channel.id)?,
                    now: now()?,
                },
                expected,
            )?;
            result.threads += 1;
        }
        result.projects = projects.len();
        if limit.is_none() {
            result.archived += self.retire_obsolete(guild, started, &mut result).await?;
            (result.orphan_deleted, result.projects_deleted) =
                self.cleanup_orphans(guild, started).await?;
        }
        Ok(result)
    }
}
