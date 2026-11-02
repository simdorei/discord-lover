use super::{MirrorSyncError, MirrorSynchronizer, db_id, names, now};
use cdr_store::mapping::{MirrorThreadUpdate, commit_new_thread_sync, project_for_channel};
use twilight_model::channel::ChannelType;

impl MirrorSynchronizer {
    /// Link only this newly created conversation. Never run global cleanup here.
    /// Caller journals the Codex ID before entering and holds ambiguous failures.
    pub async fn link_new_thread(
        &self,
        origin: u64,
        thread: &str,
        prompt: &str,
        cwd: Option<&str>,
    ) -> Result<u64, MirrorSyncError> {
        self.with_operation_deadline(
            "new_thread",
            false,
            self.link_new_thread_locked(origin, thread, prompt, cwd),
        )
        .await
    }

    async fn link_new_thread_locked(
        &self,
        origin: u64,
        thread: &str,
        prompt: &str,
        cwd: Option<&str>,
    ) -> Result<u64, MirrorSyncError> {
        let _origin_effect = cdr_store::archived_cleanup::effects::admit(
            &self.mirror_db,
            Some(db_id(origin)?),
            None,
        )?;
        let origin = self.remote.channel(origin).await?.ok_or_else(|| {
            MirrorSyncError::Invalid("new-thread origin channel is unavailable".into())
        })?;
        let guild = origin
            .guild_id
            .ok_or_else(|| MirrorSyncError::Invalid("new-thread origin has no guild".into()))?;
        if self.guild_id.is_some_and(|configured| configured != guild) {
            return Err(MirrorSyncError::Invalid(
                "new-thread origin has the wrong guild".into(),
            ));
        }
        let parent = if origin.kind.is_thread() {
            origin.parent_id.ok_or_else(|| {
                MirrorSyncError::Invalid("origin thread has no project parent".into())
            })?
        } else if origin.kind == ChannelType::GuildText {
            origin.id
        } else {
            return Err(MirrorSyncError::Invalid(
                "new requires a project text channel or its thread".into(),
            ));
        };
        let project_key =
            project_for_channel(&self.mirror_db, Some(db_id(parent)?))?.map(|(key, _)| key);
        let key = project_key
            .clone()
            .or_else(|| cwd.map(cdr_codex_state::normalize_workspace_path))
            .ok_or_else(|| {
                MirrorSyncError::Invalid("new-thread project identity is unavailable".into())
            })?;
        if key != "codex:chats"
            && !key.starts_with("projectless:")
            && cwd.is_some_and(|cwd| {
                cdr_codex_state::normalize_workspace_path(cwd)
                    != cdr_codex_state::normalize_workspace_path(&key)
            })
        {
            return Err(MirrorSyncError::Invalid(
                "new-thread project changed before room creation".into(),
            ));
        }
        let title = names::thread_name(prompt, thread);
        let (channel, expected) = self.thread_channel(guild, parent, thread, &title).await?;
        commit_new_thread_sync(
            &self.mirror_db,
            MirrorThreadUpdate {
                thread_id: thread,
                project_key: &key,
                title: &title,
                parent_id: db_id(parent)?,
                channel_id: db_id(channel.id)?,
                now: now()?,
            },
            expected,
            project_key.as_deref(),
        )?;
        Ok(channel.id)
    }
}
