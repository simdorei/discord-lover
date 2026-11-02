//! Native archival authorizes retirement, independently of old bot outcomes.
use super::{MirrorSyncError, MirrorSynchronizer, db_id, discord_id, now};
use cdr_codex_state::{CodexThreadStore, ThreadInfo};
use cdr_store::{archived_cleanup as cleanup, mapping};
use std::time::Duration;
use twilight_model::channel::ChannelType;

impl MirrorSynchronizer {
    pub(super) async fn reconcile_retirements(
        &self,
        guild: u64,
        result: &mut super::MirrorSyncResult,
    ) -> Result<(), MirrorSyncError> {
        let store = CodexThreadStore::open(&self.state_db)?;
        for mut intent in cleanup::pending(&self.mirror_db)? {
            let reconcile=async {
                let custody=self.custody(discord_id(intent.channel)?).await?;
                if intent.guild!=db_id(guild)? { return Err(invalid("cleanup guild changed")); }
                cleanup::verify_mapping(&self.mirror_db,&intent)?;
                if matches!(intent.phase.as_str(),"prepared"|"delete_rejected") {
                    let current=store.load_thread(&intent.target,true)?;
                    if current.as_ref().is_none_or(|t|!verify_archive(&store,t).as_ref().is_ok_and(|source|source==&intent.source_json)) {
                        cleanup::transition(&self.mirror_db,&intent,"aborted_source_changed","source restored or revision changed before DELETE; old work remains retired",now()?)?;
                    }
                } else {
                    if intent.phase!="deleted" {
                        if self.remote.channel(discord_id(intent.channel)?).await?.is_some() {
                            return Err(invalid("prior DELETE outcome unknown; room excluded, no repeat DELETE"));
                        }
                        intent=cleanup::transition(&self.mirror_db,&intent,"deleted","authoritative Discord 404 during reconciliation",now()?)?;
                    }
                    self.validate_before_retirement(&custody,discord_id(intent.channel)?,super::delete_guard::CleanupOutcome::PreviouslyConfirmed)?;
                    cleanup::retire_mapping(&self.mirror_db,&intent,now()?)?;
                    result.archived+=1;
                    for (family,amount) in cleanup::counts(&self.mirror_db,&intent)? {*result.records_preserved.entry(family).or_default()+=amount;}
                }
                Ok::<(),MirrorSyncError>(())
            }.await;
            if let Err(error) = reconcile {
                result
                    .skipped
                    .push(format!("room {}: {error}", intent.channel));
            }
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Keep the ordered validation and effect boundary visible in one workflow"
    )]
    pub(super) async fn retire_archived_room(
        &self,
        store: &CodexThreadStore,
        guild: u64,
        _started: f64,
        row: &mapping::StaleThread,
        archived: &ThreadInfo,
    ) -> Result<bool, MirrorSyncError> {
        let room = discord_id(row.discord_thread_id)?;
        let custody = self.custody(room).await?;
        let owners = mapping::mirror_targets(&self.mirror_db, i64::MAX)?
            .into_iter()
            .filter(|m| m.discord_thread_id == row.discord_thread_id)
            .collect::<Vec<_>>();
        let [owner] = owners.as_slice() else {
            return Err(invalid("archived cleanup ownership is ambiguous"));
        };
        let parent = discord_id(owner.discord_channel_id)?;
        self.validate_exact_mapping(&row.thread_id, room, parent)?;
        let remote = self.remote.channel(room).await?;
        match cdr_store::room_cleanup::phase(&self.mirror_db, row.discord_thread_id)?.as_deref() {
            Some("deleting") => {
                return Err(invalid(
                    "prior cleanup DELETE outcome is unknown; no repeat DELETE",
                ));
            }
            Some("deleted") if remote.is_some() => {
                return Err(invalid(
                    "confirmed tombstone contradicts present room; no repeat DELETE",
                ));
            }
            _ => {}
        }
        if let Some(channel) = &remote {
            if channel.id != room {
                return Err(invalid("archived cleanup Discord identity mismatch"));
            }
            super::channels::validate(channel, guild, ChannelType::PublicThread, Some(parent))?;
            let bot_owned = self
                .remote
                .thread_inventory(guild, parent)
                .await?
                .into_iter()
                .any(|entry| entry.channel == *channel && entry.owned_by_bot);
            if !bot_owned {
                return Err(invalid("archived cleanup room is not proven bot-owned"));
            }
        }
        let mut intent=cleanup::prepare(&self.mirror_db,&cleanup::Prepare {
            channel:db_id(room)?,target:&row.thread_id,parent:db_id(parent)?,guild:db_id(guild)?,
            origin:&super::SYNC_ORIGIN.try_with(Clone::clone).unwrap_or_else(|_|serde_json::json!({"command":"mirror sync","origin":"direct maintenance API"})),now:now()?,
        },|| { custody.validate().map_err(|e|cdr_store::StoreError::Integrity(e.to_string()))?; verify_archive(store,archived) })?;
        if matches!(intent.phase.as_str(), "delete_attempted" | "delete_unknown") {
            if remote.is_some() {
                return Err(invalid(&format!(
                    "room {room} operation {} DELETE outcome unknown; no retry",
                    intent.operation_id
                )));
            }
            intent = cleanup::transition(
                &self.mirror_db,
                &intent,
                "deleted",
                "authoritative Discord 404",
                now()?,
            )?;
        }
        if intent.phase == "deleted" {
            self.validate_before_retirement(
                &custody,
                room,
                super::delete_guard::CleanupOutcome::PreviouslyConfirmed,
            )?;
            cleanup::retire_mapping(&self.mirror_db, &intent, now()?)?;
            return Ok(true);
        }
        if !matches!(intent.phase.as_str(), "prepared" | "delete_rejected") {
            return Err(invalid(&format!(
                "room {room} operation {} held in {}",
                intent.operation_id, intent.phase
            )));
        }
        tokio::time::timeout(Duration::from_secs(35), async {
            while cleanup::effects::in_flight(&self.mirror_db, intent.channel, &intent.target)? > 0
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<(), cdr_store::StoreError>(())
        })
        .await
        .map_err(|_| invalid("dispatch drain timed out; prepared intent retained, no DELETE"))??;
        // Recheck after slow lookups and dispatch draining, before DELETE.
        let current = self.remote.channel(room).await?;
        if let Some(channel) = &current {
            if channel.id != room {
                return Err(invalid("archived cleanup Discord identity mismatch"));
            }
            super::channels::validate(channel, guild, ChannelType::PublicThread, Some(parent))?;
            if !self
                .remote
                .thread_inventory(guild, parent)
                .await?
                .into_iter()
                .any(|entry| entry.channel == *channel && entry.owned_by_bot)
            {
                return Err(invalid(
                    "archived cleanup bot ownership changed before DELETE",
                ));
            }
        }
        cleanup::verify_mapping(&self.mirror_db, &intent)?;
        if !verify_archive(store, archived)
            .as_ref()
            .is_ok_and(|source| source == &intent.source_json)
        {
            cleanup::transition(
                &self.mirror_db,
                &intent,
                "aborted_source_changed",
                "native source changed before DELETE",
                now()?,
            )?;
            return Err(invalid(
                "native source changed before DELETE; room preserved",
            ));
        }
        if current.is_some() {
            let attempt = cleanup::transition_checked(
                &self.mirror_db,
                &intent,
                "delete_attempted",
                "final archived revision validated; DELETE boundary",
                now()?,
                || {
                    custody
                        .validate()
                        .map_err(|e| cdr_store::StoreError::Integrity(e.to_string()))?;
                    if verify_archive(store, archived)? != intent.source_json {
                        return Err(cdr_store::StoreError::Integrity(
                            "native source changed before DELETE".into(),
                        ));
                    }
                    Ok(())
                },
            );
            intent = match attempt {
                Ok(intent) => intent,
                Err(error) => {
                    if !verify_archive(store, archived)
                        .as_ref()
                        .is_ok_and(|source| source == &intent.source_json)
                    {
                        cleanup::transition(
                            &self.mirror_db,
                            &intent,
                            "aborted_source_changed",
                            "source changed at final DELETE boundary",
                            now()?,
                        )?;
                    }
                    return Err(error.into());
                }
            };
            match self.remote.delete(room).await {
                Ok(()) => {
                    intent = cleanup::transition(
                        &self.mirror_db,
                        &intent,
                        "deleted",
                        "Discord confirmed deletion",
                        now()?,
                    )?;
                }
                Err(error @ MirrorSyncError::DeleteRejected(_)) => {
                    cleanup::transition(
                        &self.mirror_db,
                        &intent,
                        "delete_rejected",
                        &error.to_string(),
                        now()?,
                    )?;
                    return Err(error);
                }
                Err(error) => {
                    cleanup::transition(
                        &self.mirror_db,
                        &intent,
                        "delete_unknown",
                        &error.to_string(),
                        now()?,
                    )?;
                    return Err(invalid(&format!(
                        "room {room} operation {} DELETE outcome unknown; no automatic retry",
                        intent.operation_id
                    )));
                }
            }
        } else {
            intent = cleanup::transition(
                &self.mirror_db,
                &intent,
                "deleted",
                "authoritative Discord 404; no DELETE needed",
                now()?,
            )?;
        }
        self.validate_before_retirement(
            &custody,
            room,
            super::delete_guard::CleanupOutcome::PreviouslyConfirmed,
        )?;
        cleanup::retire_mapping(&self.mirror_db, &intent, now()?)?;
        Ok(true)
    }
}

fn invalid(message: &str) -> MirrorSyncError {
    MirrorSyncError::Invalid(message.into())
}

fn verify_archive(store: &CodexThreadStore, expected: &ThreadInfo) -> cdr_store::Result<String> {
    let invalid = || {
        cdr_store::StoreError::Integrity(
            "archived source changed or unavailable; no DELETE authorized".into(),
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
    {
        return Err(invalid());
    }
    Ok(serde_json::json!({"thread_id":current.id,"archived":true,"archived_at":current.archived_at,"rollout_path":current.rollout_path,"state_db":store.path(),"policy":"archived-mirror-retirement-v1"}).to_string())
}
