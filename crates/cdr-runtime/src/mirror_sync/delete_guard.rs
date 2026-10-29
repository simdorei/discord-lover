use super::{MirrorSyncError, MirrorSynchronizer, db_id, now};

#[derive(Clone, Copy, Debug)]
pub(super) enum CleanupOutcome {
    Deleted,
    PreviouslyConfirmed,
    ObservedMissing,
}

impl MirrorSynchronizer {
    #[allow(
        clippy::unused_self,
        reason = "the test-only per-instance barrier observes the actual caller retirement boundary"
    )]
    pub(super) fn validate_before_retirement(
        &self,
        custody: &crate::agy::ownership::CleanupPermit,
        channel: u64,
        outcome: CleanupOutcome,
    ) -> Result<(), MirrorSyncError> {
        #[cfg(test)]
        if let Some(hook) = &self.before_retirement {
            hook(channel);
        }
        custody.validate().map_err(|_| {
            let detail = match outcome {
                CleanupOutcome::Deleted => "DELETE dispatched=true; remote reported success",
                CleanupOutcome::PreviouslyConfirmed => "DELETE dispatched=false in this attempt; resuming previously confirmed cleanup",
                CleanupOutcome::ObservedMissing => "DELETE dispatched=false; remote absence observed without deletion",
            };
            MirrorSyncError::Invalid(format!(
                "room {channel} {detail}; ownership changed before mapping retirement; confirmed deleted fence and mapping retained, manual reconciliation required"
            ))
        })
    }
    pub(super) fn ensure_cleanup_reconciled(&self) -> Result<(), MirrorSyncError> {
        let rooms = cdr_store::room_cleanup::unconfirmed_channels(&self.mirror_db)?;
        if !rooms.is_empty() {
            return Err(MirrorSyncError::Invalid(format!(
                "cleanup outcome is unconfirmed and fenced for rooms {rooms:?}; manual reconciliation required before sync"
            )));
        }
        Ok(())
    }

    pub(super) fn fence_missing_room(
        &self,
        channel: u64,
        target: Option<&str>,
        custody: &crate::agy::ownership::CleanupPermit,
    ) -> Result<CleanupOutcome, MirrorSyncError> {
        validate_custody(custody, channel)?;
        let id = db_id(channel)?;
        match cdr_store::room_cleanup::phase(&self.mirror_db, id)?.as_deref() {
            Some("deleted") => Ok(CleanupOutcome::PreviouslyConfirmed),
            Some(_) => Err(MirrorSyncError::Invalid(
                "previous cleanup outcome is unconfirmed; mapping retained".into(),
            )),
            None => {
                let token = cdr_store::room_cleanup::begin(&self.mirror_db, id, target, now()?)
                    .map_err(cleanup_begin_error)?;
                validate_custody(custody, channel)?;
                cdr_store::room_cleanup::complete(&self.mirror_db, id, &token)?;
                Ok(CleanupOutcome::ObservedMissing)
            }
        }
    }
    pub(super) async fn delete_guarded(
        &self,
        channel: u64,
        target: Option<&str>,
        custody: &crate::agy::ownership::CleanupPermit,
    ) -> Result<CleanupOutcome, MirrorSyncError> {
        validate_custody(custody, channel)?;
        let id = db_id(channel)?;
        let token = cdr_store::room_cleanup::begin(&self.mirror_db, id, target, now()?)
            .map_err(cleanup_begin_error)?;
        self.dispatch_guarded_delete(channel, &token, custody).await
    }

    pub(super) async fn dispatch_guarded_delete(
        &self,
        channel: u64,
        token: &str,
        custody: &crate::agy::ownership::CleanupPermit,
    ) -> Result<CleanupOutcome, MirrorSyncError> {
        let id = db_id(channel)?;
        validate_custody(custody, channel)?;
        match self.remote.delete(channel).await {
            Ok(()) => {
                validate_after_dispatch(custody, channel, "remote reported success")?;
                cdr_store::room_cleanup::complete(&self.mirror_db, id, token)?;
            }
            Err(error @ MirrorSyncError::DeleteRejected(_)) => {
                validate_after_dispatch(custody, channel, "remote rejected deletion")?;
                cdr_store::room_cleanup::release_rejected(&self.mirror_db, id, token)?;
                return Err(error);
            }
            Err(error) => {
                return Err(MirrorSyncError::Invalid(format!(
                    "room {channel} cleanup outcome is unconfirmed; durable fence retained, no automatic retry: {error}"
                )));
            }
        }
        Ok(CleanupOutcome::Deleted)
    }
}

fn validate_after_dispatch(
    custody: &crate::agy::ownership::CleanupPermit,
    channel: u64,
    outcome: &str,
) -> Result<(), MirrorSyncError> {
    custody.validate().map_err(|_| MirrorSyncError::Invalid(format!(
        "room {channel} DELETE dispatched=true; {outcome}; ownership changed after dispatch; durable fence and mapping retained, manual reconciliation required"
    )))
}

pub(super) fn validate_custody(
    custody: &crate::agy::ownership::CleanupPermit,
    channel: u64,
) -> Result<(), MirrorSyncError> {
    custody
        .validate()
        .map_err(|_| MirrorSyncError::CleanupProtected {
            channel,
            reason: "ownership changed during cleanup",
        })
}

// Only the transaction's pre-delete pending check proves this attempt did not
// dispatch deletion. Mapping, database, and post-delete failures stay uncertain.
pub(super) fn cleanup_begin_error(error: cdr_store::StoreError) -> MirrorSyncError {
    match error {
        cdr_store::StoreError::CleanupProtected { channel, reason } if channel > 0 => {
            MirrorSyncError::CleanupProtected {
                channel: channel.unsigned_abs(),
                reason,
            }
        }
        error => MirrorSyncError::Store(error),
    }
}

#[cfg(test)]
mod retirement_tests;
#[cfg(test)]
mod tests;
