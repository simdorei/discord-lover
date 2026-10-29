//! Archived source + exact non-dispatch evidence, not a blanket ingress waiver.
use super::{MirrorSyncError, MirrorSynchronizer, db_id, discord_id, now};
use cdr_codex_state::{CodexThreadStore, ThreadInfo};
use cdr_store::{mapping, room_cleanup};
use twilight_model::channel::ChannelType;

impl MirrorSynchronizer {
    pub(super) async fn retire_archived_rejections(
        &self,
        store: &CodexThreadStore,
        guild: u64,
        started: f64,
        row: &mapping::StaleThread,
        archived: &ThreadInfo,
    ) -> Result<bool, MirrorSyncError> {
        let room = discord_id(row.discord_thread_id)?;
        let custody = self.custody(room).await?;
        if let Some(reason) = room_cleanup::archived_rejections::pending_reason(
            &self.mirror_db,
            row.discord_thread_id,
            &row.thread_id,
            now()?,
        )? {
            return Err(MirrorSyncError::CleanupProtected {
                channel: room,
                reason,
            });
        }
        let mappings = mapping::mirror_targets(&self.mirror_db, i64::MAX)?;
        let owners = mappings
            .iter()
            .filter(|m| m.discord_thread_id == row.discord_thread_id)
            .collect::<Vec<_>>();
        let [owner] = owners.as_slice() else {
            return Err(MirrorSyncError::Invalid(
                "archived cleanup room ownership is ambiguous".into(),
            ));
        };
        let parent = discord_id(owner.discord_channel_id)?;
        self.validate_exact_mapping(&row.thread_id, room, parent)?;
        let remote = self.remote.channel(room).await?;
        if let Some(channel) = &remote {
            if channel.id != room {
                return Err(MirrorSyncError::Invalid(
                    "archived cleanup Discord identity mismatch".into(),
                ));
            }
            super::channels::validate(channel, guild, ChannelType::PublicThread, Some(parent))?;
        }
        self.validate_exact_mapping(&row.thread_id, room, parent)?;
        super::delete_guard::validate_custody(&custody, room)?;
        let token = room_cleanup::archived_rejections::begin(
            &self.mirror_db,
            db_id(room)?,
            &row.thread_id,
            db_id(parent)?,
            now()?,
            || verify_archive(store, archived),
        )
        .map_err(super::delete_guard::cleanup_begin_error)?;
        let outcome = if remote.is_some() {
            self.dispatch_guarded_delete(room, &token, &custody).await?
        } else {
            super::delete_guard::validate_custody(&custody, room)?;
            room_cleanup::complete(&self.mirror_db, db_id(room)?, &token)?;
            super::delete_guard::CleanupOutcome::ObservedMissing
        };
        self.validate_before_retirement(&custody, room, outcome)?;
        if !mapping::retire_thread_sync(
            &self.mirror_db,
            &row.thread_id,
            row.discord_thread_id,
            started,
        )? {
            return Err(MirrorSyncError::Invalid(
                "archived cleanup mapping changed after confirmed deletion; preserved".into(),
            ));
        }
        Ok(true)
    }
}

fn verify_archive(store: &CodexThreadStore, expected: &ThreadInfo) -> cdr_store::Result<String> {
    let invalid = || {
        cdr_store::StoreError::Integrity(
            "archived source changed or evidence unavailable; no deletion dispatched".into(),
        )
    };
    if store
        .load_thread(&expected.id, false)
        .map_err(|_| invalid())?
        .is_some()
    {
        return Err(invalid());
    }
    let current = store
        .load_thread(&expected.id, true)
        .map_err(|_| invalid())?
        .ok_or_else(invalid)?;
    if current.archived_at <= 0
        || current.archived_at != expected.archived_at
        || current.rollout_path != expected.rollout_path
        || !current.rollout_path.is_file()
    {
        return Err(invalid());
    }
    Ok(serde_json::json!({
        "thread_id": current.id,
        "archived": true,
        "archived_at": current.archived_at,
        "rollout_path": current.rollout_path,
        "state_db": store.path(),
        "notification_confirmation": "unknown-preserved",
        "policy": "archived-expired-steer-non-dispatch-v1"
    })
    .to_string())
}
