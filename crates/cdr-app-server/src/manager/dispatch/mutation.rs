//! A committed dispatch intent outlives caller cancellation. No Drop writes.
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

use super::ResidentAppServer;
use crate::{AppServerError, RequestId, requests::is_observational};
use serde_json::Value;

pub(super) struct Attempt<'a> {
    server: &'a ResidentAppServer,
    generation: u64,
    method: &'a str,
    params: &'a Value,
    queue_claim: Option<&'a Value>,
    stop_claim: Option<&'a Value>,
    origin: Option<Value>,
    id: String,
    request_id: Mutex<Option<RequestId>>,
    started: AtomicBool,
    scoped: bool,
    effect: Mutex<Option<Box<dyn Send + Sync>>>,
}

impl<'a> Attempt<'a> {
    pub(super) fn new(
        server: &'a ResidentAppServer,
        generation: u64,
        method: &'a str,
        params: &'a Value,
        repair: bool,
        queue_claim: Option<&'a Value>,
        stop_claim: Option<&'a Value>,
    ) -> Self {
        let identified = params
            .get("threadId")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.trim().is_empty());
        let known = matches!(
            method,
            "thread/resume"
                | "thread/settings/update"
                | "turn/start"
                | "turn/steer"
                | "thread/archive"
                | "thread/backgroundTerminals/clean"
                | "thread/unsubscribe"
        ) || (repair && method == "mcpServer/tool/call");
        Self {
            server,
            generation,
            method,
            params,
            queue_claim,
            stop_claim,
            id: uuid::Uuid::new_v4().to_string(),
            origin: super::origin::current(),
            request_id: Mutex::new(None),
            started: AtomicBool::new(false),
            scoped: identified && (known || stop_claim.is_some()),
            effect: Mutex::new(None),
        }
    }

    pub(super) fn begin(&self, wire_id: &RequestId) -> Result<(), AppServerError> {
        if !is_observational(self.method)
            && let Some(fence) = &self.server.dead_generation_fence
        {
            *self.effect.lock().expect("effect permit") =
                fence.admit_effect(self.method, self.params)?;
        }
        // Interrupt remains an explicit control, not authority to clear an old
        // mutation. Its existing dispatch/transport protection is unchanged.
        if is_observational(self.method)
            || (self.method == "turn/interrupt" && self.stop_claim.is_none())
        {
            return Ok(());
        }
        if let Some(fence) = &self.server.dead_generation_fence {
            let owner = (self.server.instance_id(), self.generation);
            let request = (&*self.id, wire_id);
            let committed = if let Some(claim) = self.stop_claim {
                fence.begin_stop_mutation(owner, request, self.params, claim)?
            } else if let Some(claim) = self.queue_claim {
                fence.begin_queue_mutation(owner, request, self.method, self.params, claim)?
            } else {
                fence.begin_mutation_with_origin(
                    owner,
                    request,
                    self.method,
                    self.params,
                    self.scoped,
                    self.origin.as_ref(),
                )?
            };
            if committed {
                *self.request_id.lock().expect("mutation attempt") = Some(wire_id.clone());
            }
        }
        Ok(())
    }

    pub(super) fn write_started(&self) {
        self.started.store(true, Ordering::Release);
    }
    pub(super) fn was_started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    pub(super) fn isolates_target(&self) -> bool {
        self.scoped && self.request_id.lock().expect("mutation attempt").is_some()
    }

    pub(super) fn finish(
        &self,
        result: Result<Value, AppServerError>,
    ) -> Result<Value, AppServerError> {
        let wire_id = self.request_id.lock().expect("mutation attempt").clone();
        let (Some(wire_id), Some(fence)) = (wire_id, &self.server.dead_generation_fence) else {
            return result;
        };
        let outcome = if !self.was_started() {
            Some("not_sent")
        } else if result.is_ok() {
            Some("reply_ok")
        } else if matches!(result, Err(AppServerError::Remote { .. })) {
            Some("reply_error")
        } else {
            None
        };
        if let Some(outcome) = outcome {
            let recorded = if let Some(claim) = self.stop_claim {
                fence.finish_stop_mutation(
                    (self.server.instance_id(), self.generation),
                    (&self.id, &wire_id),
                    outcome,
                    claim,
                )
            } else {
                fence.finish_mutation(
                    (self.server.instance_id(), self.generation),
                    (&self.id, &wire_id),
                    outcome,
                )
            };
            recorded.map_err(|error| {
                self.unknown(&format!(
                    "response/dispatch evidence could not be committed: {error}"
                ))
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
                "{reason}; durable attempt {} retained; no automatic replay",
                self.id
            ),
        }
    }
}
