use std::sync::atomic::{AtomicBool, Ordering};

use super::{MutationPermit, ResidentAppServer};
use crate::{AppServerError, ServerRequest};
use serde_json::Value;

pub(in crate::manager) struct ResponsePermit {
    pub(super) permit: Option<MutationPermit>,
    pub(super) original: ServerRequest,
    pub(super) authority: Option<Value>,
}

/// Durable admission is committed in the writer, never by a Drop task.
pub(in crate::manager) struct ResponseAttempt<'a> {
    server: &'a ResidentAppServer,
    generation: u64,
    permit: ResponsePermit,
    payload: Value,
    admitted: AtomicBool,
    started: AtomicBool,
    effect: std::sync::Mutex<Option<Box<dyn Send + Sync>>>,
}

impl<'a> ResponseAttempt<'a> {
    pub(in crate::manager) fn new(
        server: &'a ResidentAppServer,
        generation: u64,
        permit: ResponsePermit,
        payload: Value,
    ) -> Self {
        Self {
            server,
            generation,
            permit,
            payload,
            admitted: AtomicBool::new(false),
            started: AtomicBool::new(false),
            effect: std::sync::Mutex::new(None),
        }
    }

    pub(in crate::manager) fn begin(&self) -> Result<(), AppServerError> {
        if let Some(fence) = &self.server.dead_generation_fence {
            *self.effect.lock().expect("response effect permit") =
                fence.admit_effect("server/response", &self.permit.original.params)?;
        }
        self.server.check_actual_mutation(
            self.permit.permit.as_ref(),
            self.generation,
            "server/response",
            &self.permit.original.params,
        )?;
        if let (Some(fence), Some(authority)) =
            (&self.server.dead_generation_fence, &self.permit.authority)
        {
            fence.begin_response(
                (self.server.instance_id(), self.generation),
                &self.permit.original,
                authority,
                &self.payload,
            )?;
            self.admitted.store(true, Ordering::Release);
        }
        Ok(())
    }

    pub(in crate::manager) fn write_started(&self) {
        self.started.store(true, Ordering::Release);
    }

    pub(in crate::manager) fn finish(
        &self,
        result: Result<(), AppServerError>,
    ) -> Result<(), AppServerError> {
        if !self.admitted.load(Ordering::Acquire) {
            return result;
        }
        let outcome = if !self.started.load(Ordering::Acquire) {
            Some("not_sent")
        } else if result.is_ok() {
            Some("flushed")
        } else {
            None
        };
        if let (Some(outcome), Some(fence), Some(authority)) = (
            outcome,
            &self.server.dead_generation_fence,
            &self.permit.authority,
        ) {
            fence
                .finish_response(
                    (self.server.instance_id(), self.generation),
                    &self.permit.original,
                    authority,
                    &self.payload,
                    outcome,
                )
                .map_err(|error| unknown(&error.to_string()))?;
            result
        } else {
            result.map_err(|error| unknown(&error.to_string()))
        }
    }
}

fn unknown(reason: &str) -> AppServerError {
    AppServerError::MutationOutcomeUnknown {
        method: "server/response".into(),
        reason: format!("{reason}; original response admission retained; no automatic replay"),
    }
}
