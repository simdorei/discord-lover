use super::{MirrorSyncError, MirrorSynchronizer, discord_id};
use cdr_codex_state::CodexThreadStore;
use cdr_store::mapping::{StaleThread, mirror_targets, retire_thread_sync, stale_threads};
use std::collections::BTreeSet;
use twilight_model::channel::ChannelType;

impl MirrorSynchronizer {
    pub(super) async fn retire_obsolete(
        &self,
        guild: u64,
        started: f64,
    ) -> Result<usize, MirrorSyncError> {
        let store = CodexThreadStore::open(&self.state_db)?;
        // Only a successful, unfiltered active inventory establishes absence.
        // Missing rollout files do not remove an existing identity from this set.
        let retained = store
            .load_recent_threads(0)?
            .into_iter()
            .map(|thread| thread.id)
            .collect::<BTreeSet<_>>();
        let mappings = mirror_targets(&self.mirror_db, i64::MAX)?;
        let shared = mappings
            .iter()
            .filter(|row| retained.contains(&row.codex_thread_id))
            .map(|row| row.discord_thread_id)
            .collect::<BTreeSet<_>>();
        let mut count = 0;
        for row in stale_threads(&self.mirror_db, &retained, started)? {
            if store.load_thread(&row.thread_id, false)?.is_some() {
                continue;
            }
            // Duplicate ownership needs explicit reconciliation. Mapping-only
            // deletion would bypass the pending-work fence and race with ingress.
            if shared.contains(&row.discord_thread_id) {
                return Err(MirrorSyncError::Invalid(format!(
                    "shared Discord room {} has an active mapping; room and mappings preserved; resolve duplicate ownership before sync",
                    row.discord_thread_id
                )));
            }
            count += usize::from(
                self.retire_obsolete_row(&store, guild, started, &row, None)
                    .await?,
            );
        }
        Ok(count)
    }

    pub(super) async fn retire_obsolete_row(
        &self,
        store: &CodexThreadStore,
        guild: u64,
        started: f64,
        row: &StaleThread,
        parent: Option<u64>,
    ) -> Result<bool, MirrorSyncError> {
        // Only archived mapped rooms may reconcile exact expired Steer
        // non-dispatch evidence. Ordinary, orphan and absent-source paths stay strict.
        if parent.is_none()
            && cdr_store::room_cleanup::pending_reason(
                &self.mirror_db,
                row.discord_thread_id,
                Some(&row.thread_id),
            )? == Some("ingress")
            && let Some(archived) = store.load_thread(&row.thread_id, true)?
        {
            return self
                .retire_archived_rejections(store, guild, started, row, &archived)
                .await;
        }
        // A missing Discord room is not permission to drop its pending work mapping.
        let custody = self.custody(discord_id(row.discord_thread_id)?).await?;
        self.ensure_no_requests(discord_id(row.discord_thread_id)?)?;
        let channel = self
            .remote
            .channel(discord_id(row.discord_thread_id)?)
            .await?;
        // A newly restored identity must survive a slow Discord lookup.
        if store.load_thread(&row.thread_id, false)?.is_some()
            || (parent.is_some() && store.load_thread(&row.thread_id, true)?.is_some())
        {
            return Ok(false);
        }
        if let Some(parent) = parent {
            self.validate_exact_mapping(
                &row.thread_id,
                discord_id(row.discord_thread_id)?,
                parent,
            )?;
        }
        let outcome = if let Some(channel) = channel {
            super::channels::validate(&channel, guild, ChannelType::PublicThread, parent)?;
            self.ensure_no_requests(channel.id)?;
            self.delete_guarded(channel.id, Some(&row.thread_id), &custody)
                .await?
        } else {
            self.fence_missing_room(
                discord_id(row.discord_thread_id)?,
                Some(&row.thread_id),
                &custody,
            )?
        };
        // Retire the mapping only after the approved Discord deletion succeeds.
        self.validate_before_retirement(&custody, discord_id(row.discord_thread_id)?, outcome)?;
        if !retire_thread_sync(
            &self.mirror_db,
            &row.thread_id,
            row.discord_thread_id,
            started,
        )? {
            return Err(MirrorSyncError::Invalid(format!(
                "mapping {} changed during cleanup; run sync again",
                row.thread_id
            )));
        }
        Ok(true)
    }
}
