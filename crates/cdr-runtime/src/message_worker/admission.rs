use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use cdr_discord::gateway::ingress::MessageGapFenceError;
use cdr_store::StoreError;
use cdr_store::ingress::{admit, record_processing_mode};
use thiserror::Error;
use twilight_model::channel::Message;

use super::classification::MessageCandidate;
use super::custody::{self, MessageCustody};
use crate::message_plan::{MessagePlan, MessagePlanError};
use crate::restart_readiness::drain::AdmissionPermit;

#[derive(Debug, Error)]
pub enum MessageAdmissionError {
    #[error("room {channel} ownership changed; history progress retained")]
    OwnershipChanged { channel: u64 },
    #[error("Discord identifier does not fit the SQLite integer contract")]
    IntegerRange,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    MessageGapFence(#[from] MessageGapFenceError),
    #[error(
        "admitted Discord message database mismatch: claimed={claimed_database:?} context={context_database:?}"
    )]
    DatabaseMismatch {
        claimed_database: PathBuf,
        context_database: PathBuf,
    },
}

pub(crate) struct AdmittedMessage {
    message: Box<Message>,
    database: PathBuf,
    persisted_id: i64,
    channel_id: u64,
    user_id: u64,
    frozen_plan: Result<MessagePlan, MessagePlanError>,
    processing_mode: MessageProcessingMode,
    custody: Box<MessageCustody>,
    admission_permit: Option<AdmissionPermit>,
}

pub(super) struct ProcessingParts {
    pub message: Box<Message>,
    pub channel_id: u64,
    pub user_id: u64,
    pub frozen_plan: Result<MessagePlan, MessagePlanError>,
    pub processing_mode: MessageProcessingMode,
    pub custody: Box<MessageCustody>,
    pub admission_permit: Option<AdmissionPermit>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MessageProcessingMode {
    Normal,
    PendingReplyOnly,
}

impl AdmittedMessage {
    pub(crate) fn retain_admission(mut self, permit: Option<&AdmissionPermit>) -> Self {
        self.admission_permit = permit.cloned();
        self
    }

    pub(crate) fn require_pending_reply(mut self) -> Result<Self, MessageAdmissionError> {
        record_processing_mode(
            &self.database,
            &format!("message:{}", self.persisted_id),
            "pending_reply_only",
        )?;
        self.processing_mode = MessageProcessingMode::PendingReplyOnly;
        Ok(self)
    }

    #[cfg(test)]
    pub(crate) fn is_pending_reply_only(&self) -> bool {
        self.processing_mode == MessageProcessingMode::PendingReplyOnly
    }

    pub(crate) fn verify_database_affinity(
        self,
        context_database: &Path,
    ) -> Result<Self, MessageAdmissionError> {
        if self.database != context_database {
            return Err(MessageAdmissionError::DatabaseMismatch {
                claimed_database: self.database,
                context_database: context_database.to_path_buf(),
            });
        }
        Ok(self)
    }

    pub(super) fn into_processing_parts(
        self,
        context_database: &Path,
    ) -> Result<ProcessingParts, MessageAdmissionError> {
        let admitted = self.verify_database_affinity(context_database)?;
        debug_assert_eq!(
            i64::try_from(admitted.message.id.get()).ok(),
            Some(admitted.persisted_id)
        );
        Ok(ProcessingParts {
            message: admitted.message,
            channel_id: admitted.channel_id,
            user_id: admitted.user_id,
            frozen_plan: admitted.frozen_plan,
            processing_mode: admitted.processing_mode,
            custody: admitted.custody,
            admission_permit: admitted.admission_permit,
        })
    }
}

impl MessageAdmissionError {
    pub(crate) fn is_database_mismatch(&self) -> bool {
        matches!(self, Self::DatabaseMismatch { .. })
    }

    pub(crate) fn is_gap_fence_advanced(&self) -> bool {
        matches!(
            self,
            Self::MessageGapFence(MessageGapFenceError::Advanced { .. })
        )
    }
}

pub(crate) fn admit_message_candidate_at(
    candidate: MessageCandidate,
    observed_at: SystemTime,
) -> Result<Option<AdmittedMessage>, MessageAdmissionError> {
    let parts = candidate.into_admission_parts();
    let observed_at = observed_at
        .duration_since(UNIX_EPOCH)
        .map_err(StoreError::from)?
        .as_secs_f64();
    let request = custody::request(&parts, observed_at)?;
    let admission = admit(&parts.database, &request)?;
    if !admission.created {
        return Ok(None);
    }
    // The next-message !new reservation is consumed atomically by admission.
    // Execute that persisted decision, never the stale pre-admission Ask plan.
    let frozen_plan = if let Some(record) = &admission.record
        && (record.payload.get("new_prompt_arm").is_some()
            || record.payload.get("new_prompt_arm_ref").is_some()
            || record.payload["new_prompt_mention_arm"].is_string())
    {
        if let Some(text) = record
            .payload
            .pointer("/plan/Respond")
            .and_then(serde_json::Value::as_str)
        {
            Ok(MessagePlan::Respond(text.to_owned()))
        } else if let Some(prompt) = record
            .payload
            .pointer("/plan/Execute/New/prompt")
            .and_then(serde_json::Value::as_str)
        {
            Ok(MessagePlan::Execute(
                crate::command_plan::CommandAction::New {
                    prompt: prompt.to_owned(),
                },
            ))
        } else {
            return Err(StoreError::Integrity("invalid persisted !new decision".into()).into());
        }
    } else {
        parts.frozen_plan
    };
    let custody = Box::new(MessageCustody::new(
        parts.database.clone(),
        request.ingress_id,
    ));
    Ok(Some(AdmittedMessage {
        message: parts.message,
        database: parts.database,
        persisted_id: parts.persisted_id,
        channel_id: parts.channel_id,
        user_id: parts.user_id,
        frozen_plan,
        processing_mode: MessageProcessingMode::Normal,
        custody,
        admission_permit: None,
    }))
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "admission_boundary_tests.rs"]
mod boundary_tests;

#[cfg(test)]
#[path = "admission_race_tests.rs"]
mod race_tests;
