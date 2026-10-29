//! Narrow maintenance API; callers must arrange exclusive runtime ownership.
use super::{MirrorSyncError, MirrorSynchronizer, db_id, now};
use cdr_codex_state::CodexThreadStore;
use cdr_store::mapping::{StaleThread, mirror_targets, retire_thread_sync};

impl MirrorSynchronizer {
    pub async fn retire_exact_absent(
        &self,
        thread_id: &str,
        room: u64,
        parent: u64,
    ) -> Result<(), MirrorSyncError> {
        self.with_operation_deadline(
            "exact_cleanup",
            false,
            self.retire_exact_absent_locked(thread_id, room, parent),
        )
        .await
    }

    async fn retire_exact_absent_locked(
        &self,
        thread_id: &str,
        room: u64,
        parent: u64,
    ) -> Result<(), MirrorSyncError> {
        let started = now()?;
        let guild = self.guild_id.ok_or_else(|| {
            MirrorSyncError::Invalid("exact cleanup requires a pinned guild".into())
        })?;
        let store = CodexThreadStore::open(&self.state_db)?;
        // Prove inventory availability, not absence of a rollout or RPC response.
        let inventory = store.load_recent_threads(0)?;
        if inventory.iter().any(|thread| thread.id == thread_id)
            || store.load_thread(thread_id, true)?.is_some()
        {
            return Err(MirrorSyncError::Invalid(
                "exact cleanup target still exists in Codex".into(),
            ));
        }
        self.ensure_cleanup_reconciled()?;
        self.ensure_exact_pending(thread_id, room)?;
        if cdr_store::room_cleanup::confirmed_target(&self.mirror_db, db_id(room)?, thread_id)? {
            let custody = self.custody(room).await?;
            self.ensure_no_requests(room)?;
            self.ensure_exact_pending(thread_id, room)?;
            if self.remote.channel(room).await?.is_some() {
                return Err(MirrorSyncError::Invalid(
                    "confirmed deletion contradicts remote room; no repeat DELETE".into(),
                ));
            }
            if store.load_thread(thread_id, true)?.is_some() {
                return Err(MirrorSyncError::Invalid(
                    "exact target restored during verification".into(),
                ));
            }
            self.ensure_no_requests(room)?;
            self.ensure_exact_pending(thread_id, room)?;
            let mappings = mirror_targets(&self.mirror_db, i64::MAX)?;
            let stored_room = db_id(room)?;
            let related = mappings.iter().any(|row| {
                row.codex_thread_id == thread_id || row.discord_thread_id == stored_room
            });
            if !related {
                return Ok(());
            }
            self.validate_exact_mapping(thread_id, room, parent)?;
            self.validate_before_retirement(
                &custody,
                room,
                super::delete_guard::CleanupOutcome::PreviouslyConfirmed,
            )?;
            if !retire_thread_sync(&self.mirror_db, thread_id, db_id(room)?, started)? {
                return Err(MirrorSyncError::Invalid(
                    "confirmed cleanup mapping changed; preserved".into(),
                ));
            }
            return Ok(());
        }
        self.validate_exact_mapping(thread_id, room, parent)?;
        let row = StaleThread {
            thread_id: thread_id.into(),
            discord_thread_id: db_id(room)?,
            title: String::new(),
        };
        if !self
            .retire_obsolete_row(&store, guild, started, &row, Some(parent))
            .await?
        {
            return Err(MirrorSyncError::Invalid(
                "exact cleanup target reappeared; room preserved".into(),
            ));
        }
        Ok(())
    }

    fn ensure_exact_pending(&self, target: &str, room: u64) -> Result<(), MirrorSyncError> {
        if let Some(reason) =
            cdr_store::room_cleanup::pending_reason(&self.mirror_db, db_id(room)?, Some(target))?
        {
            return Err(MirrorSyncError::Invalid(format!(
                "exact cleanup protected by {reason}; preserved"
            )));
        }
        Ok(())
    }

    pub(super) fn validate_exact_mapping(
        &self,
        thread_id: &str,
        room: u64,
        parent: u64,
    ) -> Result<(), MirrorSyncError> {
        let room = db_id(room)?;
        let parent = db_id(parent)?;
        let mappings = mirror_targets(&self.mirror_db, i64::MAX)?;
        let owners = mappings
            .iter()
            .filter(|row| row.discord_thread_id == room)
            .collect::<Vec<_>>();
        if owners.len() != 1
            || owners[0].codex_thread_id != thread_id
            || owners[0].discord_channel_id != parent
        {
            return Err(MirrorSyncError::Invalid(
                "exact cleanup mapping changed or room is shared; preserved".into(),
            ));
        }
        Ok(())
    }
}
