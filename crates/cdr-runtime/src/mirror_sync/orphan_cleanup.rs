use super::{MirrorSyncError, MirrorSynchronizer, db_id, discord_id};
use cdr_store::mapping::{
    is_mirrored_channel, mirror_targets, remaining_discord_ids, stale_projects,
};
use std::collections::BTreeSet;
use twilight_model::channel::ChannelType;

impl MirrorSynchronizer {
    pub(super) fn ensure_no_requests(&self, channel: u64) -> Result<(), MirrorSyncError> {
        let id = db_id(channel)?;
        let mut targets = vec![None];
        targets.extend(
            mirror_targets(&self.mirror_db, i64::MAX)?
                .into_iter()
                .filter(|row| row.discord_thread_id == id)
                .map(|row| Some(row.codex_thread_id)),
        );
        for target in targets {
            if let Some(reason) =
                cdr_store::room_cleanup::pending_reason(&self.mirror_db, id, target.as_deref())?
            {
                return Err(MirrorSyncError::CleanupProtected { channel, reason });
            }
        }
        Ok(())
    }

    pub(super) async fn cleanup_orphans(
        &self,
        guild: u64,
        started: f64,
    ) -> Result<(usize, usize), MirrorSyncError> {
        let ids = remaining_discord_ids(&self.mirror_db)?;
        let started_ms = std::time::Duration::try_from_secs_f64(started)
            .map_err(|e| MirrorSyncError::Invalid(e.to_string()))?
            .as_millis();
        let mut deleted = 0;
        for parent in ids.project_channel_ids {
            let parent = discord_id(parent)?;
            for entry in self.remote.thread_inventory(guild, parent).await? {
                let channel = entry.channel;
                // Snowflakes created after this sync started might not have their mapping committed yet.
                let created_ms = (channel.id >> 22) + 1_420_070_400_000;
                if !entry.owned_by_bot
                    || channel.kind != ChannelType::PublicThread
                    || u128::from(created_ms) >= started_ms
                    || is_mirrored_channel(&self.mirror_db, Some(db_id(channel.id)?))?
                {
                    continue;
                }
                super::channels::validate(
                    &channel,
                    guild,
                    ChannelType::PublicThread,
                    Some(parent),
                )?;
                self.ensure_no_requests(channel.id)?;
                // Recheck immediately before the irreversible request, rather than trusting the initial inventory.
                if is_mirrored_channel(&self.mirror_db, Some(db_id(channel.id)?))? {
                    continue;
                }
                let custody = self.custody(channel.id).await?;
                self.delete_guarded(channel.id, None, &custody).await?;
                deleted += 1;
            }
        }
        let mut projects = 0;
        for row in stale_projects(&self.mirror_db, &BTreeSet::new(), started)? {
            if mirror_targets(&self.mirror_db, i64::MAX)?
                .iter()
                .any(|m| m.discord_channel_id == row.discord_channel_id)
            {
                continue;
            }
            let id = discord_id(row.discord_channel_id)?;
            let custody = self.custody(id).await?;
            self.ensure_no_requests(id)?;
            let outcome = if let Some(channel) = self.remote.channel(id).await? {
                super::channels::validate(&channel, guild, ChannelType::GuildText, None)?;
                if !self.remote.thread_inventory(guild, id).await?.is_empty() {
                    return Err(MirrorSyncError::Invalid(format!(
                        "obsolete project {id} still contains protected threads"
                    )));
                }
                self.ensure_no_requests(id)?;
                self.delete_guarded(id, None, &custody).await?
            } else {
                self.fence_missing_room(id, None, &custody)?
            };
            self.validate_before_retirement(&custody, id, outcome)?;
            if !cdr_store::mapping::retire_project_sync(
                &self.mirror_db,
                &row.project_key,
                row.discord_channel_id,
                started,
            )? {
                return Err(MirrorSyncError::Invalid(
                    "project mapping changed during cleanup".into(),
                ));
            }
            projects += 1;
        }
        Ok((deleted, projects))
    }
}
