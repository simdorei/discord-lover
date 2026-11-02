//! The managed task owns one dispatch intent even after its caller is cancelled.
use super::{Value, Work, transport::WritePhase};
use crate::{AppServerError, RequestId, requests::is_observational};
use std::sync::Mutex;

pub(super) struct Attempt<'a> {
    work: &'a Work,
    method: &'a str,
    params: &'a Value,
    id: String,
    wire: Mutex<Option<RequestId>>,
    effect: Mutex<Option<Box<dyn Send + Sync>>>,
}

impl<'a> Attempt<'a> {
    pub(super) fn new(work: &'a Work, method: &'a str, params: &'a Value) -> Self {
        Self {
            work,
            method,
            params,
            id: uuid::Uuid::new_v4().to_string(),
            wire: Mutex::new(None),
            effect: Mutex::new(None),
        }
    }

    pub(super) fn begin(&self, wire: &RequestId) -> Result<(), AppServerError> {
        if is_observational(self.method) {
            return Ok(());
        }
        if let Some(fence) = &self.work.fence {
            *self.effect.lock().expect("maintenance effect permit") =
                fence.admit_effect(self.method, self.params)?;
        }
        if let Some(fence) = &self.work.fence
            && fence.begin_mutation_with_origin(
                (&self.work.token.owner_id, self.work.token.generation),
                (&self.id, wire),
                self.method,
                self.params,
                true,
                self.work.stop_origin.as_ref(),
            )?
        {
            *self.wire.lock().expect("maintenance attempt") = Some(wire.clone());
        }
        Ok(())
    }

    pub(super) fn finish(
        &self,
        result: Result<Value, AppServerError>,
        phase: WritePhase,
    ) -> Result<Value, AppServerError> {
        let wire = self.wire.lock().expect("maintenance attempt").clone();
        let (Some(wire), Some(fence)) = (wire, &self.work.fence) else {
            return result;
        };
        let outcome = if phase == WritePhase::NotStarted {
            Some("not_sent")
        } else if result.is_ok() {
            Some("reply_ok")
        } else if matches!(result, Err(AppServerError::Remote { .. })) {
            Some("reply_error")
        } else {
            None
        };
        if let Some(outcome) = outcome {
            fence
                .finish_mutation(
                    (&self.work.token.owner_id, self.work.token.generation),
                    (&self.id, &wire),
                    outcome,
                )
                .map_err(|error| {
                    self.unknown(&format!("maintenance result not committed: {error}"))
                })?;
            result
        } else {
            result.map_err(|error| self.unknown(&error.to_string()))
        }
    }

    fn unknown(&self, reason: &str) -> AppServerError {
        AppServerError::MutationOutcomeUnknown {
            method: self.method.into(),
            reason: format!(
                "{reason}; durable maintenance attempt {} retained; no automatic replay",
                self.id
            ),
        }
    }
}
