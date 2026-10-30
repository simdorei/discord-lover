use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use cdr_store::StoreError;
use serde_json::Value;

use crate::discord_dispatch::InteractionProcessingMode;

#[derive(Clone, Copy, Debug)]
enum KnownRefusal {
    Cleanup,
    AsyncAnswer,
}

#[derive(Debug)]
pub(super) struct ExecutionCustody {
    database: PathBuf,
    ingress_id: String,
    result_recorded: bool,
    known_refusal: Option<KnownRefusal>,
    armed: bool,
}

impl ExecutionCustody {
    pub(super) fn request_rejection(
        &self,
        work: &crate::discord_dispatch::InboundInteractionWork,
    ) -> Result<Option<String>, StoreError> {
        let record =
            cdr_store::ingress::get(&self.database, &self.ingress_id)?.ok_or_else(|| {
                StoreError::Integrity("interaction admission record disappeared".into())
            })?;
        let Some(value) = record
            .payload
            .get("request_rejection")
            .filter(|v| !v.is_null())
        else {
            return Ok(None);
        };
        if u64::try_from(record.channel_id).ok() != Some(work.channel_id.get())
            || u64::try_from(record.owner_user_id).ok() != Some(work.user_id.get())
            || record.payload["work"] != serde_json::to_value(&work.work)?
        {
            return Err(StoreError::Integrity(
                "rejected interaction envelope identity changed".into(),
            ));
        }
        value
            .as_str()
            .filter(|v| !v.is_empty())
            .map(|v| Some(v.to_owned()))
            .ok_or_else(|| StoreError::Integrity("interaction rejection is malformed".into()))
    }

    pub(super) fn begin(
        worker_database: &Path,
        custody_database: &Path,
        ingress_id: &str,
        mode: InteractionProcessingMode,
    ) -> Result<Self, StoreError> {
        let database = database_affinity(worker_database, custody_database)?;
        let began = match mode {
            InteractionProcessingMode::Execute => cdr_store::ingress::begin_execution(
                &database,
                ingress_id,
                "processing",
                None,
                now()?,
            )?,
            InteractionProcessingMode::ConfirmationOnly => {
                cdr_store::ingress::begin_confirmation(&database, ingress_id, now()?)?
            }
        };
        if !began {
            return Err(StoreError::Integrity(format!(
                "interaction custody cannot begin {mode:?}: {ingress_id}"
            )));
        }
        Ok(Self {
            database,
            ingress_id: ingress_id.to_owned(),
            result_recorded: false,
            known_refusal: None,
            armed: true,
        })
    }

    pub(super) fn record_result(&mut self, outcome: &Value) -> Result<(), StoreError> {
        cdr_store::ingress::record_result(&self.database, &self.ingress_id, outcome, now()?)?;
        self.result_recorded = true;
        self.known_refusal = if cdr_store::ingress::CleanupRefusal::from_outcome(Some(outcome))
            .is_some()
        {
            Some(KnownRefusal::Cleanup)
        } else if cdr_store::ingress::AsyncAnswerRefusal::from_outcome(Some(outcome)).is_some() {
            Some(KnownRefusal::AsyncAnswer)
        } else {
            None
        };
        Ok(())
    }

    pub(super) fn finish_notification(
        &mut self,
        outcome: &Value,
    ) -> Result<(), super::InteractionWorkerError> {
        self.finish_success(outcome)
            .map_err(|error| match self.known_refusal {
                Some(KnownRefusal::AsyncAnswer) => {
                    super::InteractionWorkerError::AsyncAnswerNotice(error.to_string())
                }
                Some(KnownRefusal::Cleanup) => {
                    crate::cleanup_refusal::NotificationFailure::confirmation(
                        &self.database,
                        &self.ingress_id,
                        error,
                    )
                    .into()
                }
                None => error.into(),
            })
    }

    pub(super) fn finish_success(&mut self, outcome: &Value) -> Result<(), StoreError> {
        if !self.result_recorded {
            self.record_result(outcome)?;
        }
        cdr_store::ingress::confirm(&self.database, &self.ingress_id, now()?)?;
        self.armed = false;
        Ok(())
    }

    pub(super) fn hold_failed(&mut self) -> Result<(), StoreError> {
        if self.result_recorded {
            self.armed = false;
            return Ok(());
        }
        cdr_store::ingress::hold(
            &self.database,
            &self.ingress_id,
            "interaction_processing_failed",
            false,
            now()?,
        )?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for ExecutionCustody {
    fn drop(&mut self) {
        if !self.armed || self.result_recorded {
            return;
        }
        let now = match now() {
            Ok(now) => now,
            Err(error) => {
                eprintln!("interaction_processing_cancel_hold_clock_failed: {error}");
                return;
            }
        };
        if let Err(error) = cdr_store::ingress::hold(
            &self.database,
            &self.ingress_id,
            "interaction_processing_cancelled",
            false,
            now,
        ) {
            eprintln!("interaction_processing_cancel_hold_failed: {error}");
        }
    }
}

fn now() -> Result<f64, StoreError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64())
}

fn database_affinity(
    worker_database: &Path,
    custody_database: &Path,
) -> Result<PathBuf, StoreError> {
    let worker_database = std::fs::canonicalize(worker_database)?;
    let custody_database = std::fs::canonicalize(custody_database)?;
    if worker_database != custody_database {
        return Err(StoreError::Integrity(
            "interaction custody belongs to a different worker database".into(),
        ));
    }
    Ok(worker_database)
}

#[cfg(test)]
mod tests;
