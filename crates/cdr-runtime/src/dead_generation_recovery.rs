//! Durable fencing installed before sharing the resident app-server.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use cdr_app_server::{AppServerError, DeadGenerationFence, DeadGenerationWork, extract_thread_id};
use cdr_store::dead_generation::{
    DeadGenerationCapture, activate_runtime, capture_dead_generation, generation_is_sealed,
    target_is_held,
};
use serde_json::Value;

#[path = "dead_generation_recovery/response_authority.rs"]
mod response_authority;

pub struct RuntimeDeadGenerationFence {
    mirror_db: PathBuf,
    runtime_id: String,
    startup_channel_id: Option<i64>,
}

impl RuntimeDeadGenerationFence {
    /// Call only after acquiring the bot's single-instance guard, before any
    /// app-server clients or queue workers can run.
    pub fn new(
        mirror_db: PathBuf,
        runtime_id: String,
        startup_channel_id: Option<u64>,
    ) -> Result<Self, AppServerError> {
        let startup_channel_id = startup_channel_id
            .map(i64::try_from)
            .transpose()
            .map_err(failure)?;
        activate_runtime(&mirror_db, &runtime_id).map_err(failure)?;
        cdr_store::mutation_attempt::activate(&mirror_db, &runtime_id).map_err(failure)?;
        Ok(Self {
            mirror_db,
            runtime_id,
            startup_channel_id,
        })
    }

    fn claim_mutation(
        &self,
        owner: (&str, u64),
        request: (&str, &cdr_app_server::RequestId),
        method: &str,
        params: &Value,
        identity: (Option<&Value>, Option<&Value>),
        scoped: bool,
    ) -> Result<bool, AppServerError> {
        if method == "thread/archive" {
            let target = extract_thread_id(params)
                .ok_or_else(|| mutation_held("archive requires an exact target"))?;
            if !scoped || identity.0.is_some() {
                return Err(mutation_held("archive cannot borrow queue authority"));
            }
            cdr_store::archive_fence::validate_dispatch(
                &self.mirror_db,
                &self.runtime_id,
                &target,
                identity.1,
            )
            .map_err(mutation_held)?;
            // The archive reservation is the durable intent. Do not settle or
            // let an older mutation/response/stop hold block this explicit action.
            return Ok(false);
        }
        let wire_id = serde_json::to_string(request.1)?;
        let target = extract_thread_id(params);
        let generation = i64::try_from(owner.1).map_err(mutation_held)?;
        let (authority, origin) = identity;
        let payload = authority
            .map(|claim| {
                serde_json::json!({
                    "request": params, "queueClaim": claim,
                })
            })
            .or_else(|| {
                origin.map(|origin| {
                    serde_json::json!({
                        "request": params, "stopOrigin": origin,
                    })
                })
            });
        cdr_store::mutation_attempt::begin_checked(
            &self.mirror_db,
            &cdr_store::mutation_attempt::NewAttempt {
                runtime_id: &self.runtime_id,
                owner_id: owner.0,
                generation,
                attempt_id: request.0,
                wire_id: &wire_id,
                method,
                target_thread_id: target.as_deref(),
                scoped,
                payload: payload.as_ref().unwrap_or(params),
            },
            |connection| {
                if let Some(target) = target.as_deref() {
                    cdr_store::ingress::stop::control::require_unheld_in(connection, target)?;
                    cdr_store::mutation_attempt::response::require_unheld_in(connection, target)?;
                } else {
                    cdr_store::mutation_attempt::response::require_all_resolved_in(connection)?;
                }
                if authority.is_none() {
                    cdr_store::ingress::stop::revision::validate_request_in(
                        connection,
                        method,
                        target.as_deref(),
                        origin,
                    )?;
                }
                if let Some(claim) = authority {
                    if method != "turn/start" {
                        return Err(cdr_store::StoreError::Integrity(
                            "queue authority cannot authorize another mutation".into(),
                        ));
                    }
                    cdr_store::queue::start_authority::validate_in(
                        connection,
                        claim,
                        target.as_deref().unwrap_or(""),
                        generation,
                    )?;
                }
                Ok(())
            },
        )
        .map_err(mutation_held)?;
        Ok(true)
    }
}

impl DeadGenerationFence for RuntimeDeadGenerationFence {
    fn request_origin(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<Option<Value>, AppServerError> {
        if cdr_app_server::requests::is_observational(method) || method == "turn/interrupt" {
            return Ok(None);
        }
        cdr_store::ingress::stop::revision::capture(
            &self.mirror_db,
            extract_thread_id(params).as_deref(),
        )
        .map(Some)
        .map_err(mutation_held)
    }

    fn begin_mutation_with_origin(
        &self,
        owner: (&str, u64),
        request: (&str, &cdr_app_server::RequestId),
        method: &str,
        params: &Value,
        scoped: bool,
        origin: Option<&Value>,
    ) -> Result<bool, AppServerError> {
        self.claim_mutation(owner, request, method, params, (None, origin), scoped)
    }

    fn response_authority(
        &self,
        owner: (&str, u64),
        request: &cdr_app_server::ServerRequest,
    ) -> Result<Option<Value>, AppServerError> {
        response_authority::capture(self, owner, request).map(Some)
    }

    fn begin_response(
        &self,
        owner: (&str, u64),
        request: &cdr_app_server::ServerRequest,
        authority: &Value,
        payload: &Value,
    ) -> Result<(), AppServerError> {
        response_authority::begin(self, owner, request, authority, payload)
    }

    fn finish_response(
        &self,
        owner: (&str, u64),
        request: &cdr_app_server::ServerRequest,
        authority: &Value,
        payload: &Value,
        outcome: &str,
    ) -> Result<(), AppServerError> {
        response_authority::finish(self, owner, request, authority, payload, outcome)
    }

    fn begin_mutation(
        &self,
        owner: (&str, u64),
        request: (&str, &cdr_app_server::RequestId),
        method: &str,
        params: &Value,
        scoped: bool,
    ) -> Result<bool, AppServerError> {
        self.claim_mutation(owner, request, method, params, (None, None), scoped)
    }

    fn begin_queue_mutation(
        &self,
        owner: (&str, u64),
        request: (&str, &cdr_app_server::RequestId),
        method: &str,
        params: &Value,
        claim: &Value,
    ) -> Result<bool, AppServerError> {
        self.claim_mutation(owner, request, method, params, (Some(claim), None), true)
    }

    fn begin_stop_mutation(
        &self,
        owner: (&str, u64),
        request: (&str, &cdr_app_server::RequestId),
        params: &Value,
        claim: &Value,
    ) -> Result<bool, AppServerError> {
        cdr_store::ingress::stop::control::begin_wire(
            &self.mirror_db,
            claim,
            (owner.0, i64::try_from(owner.1).map_err(mutation_held)?),
            (request.0, &serde_json::to_string(request.1)?),
            params,
        )
        .map_err(mutation_held)?;
        Ok(true)
    }

    fn finish_stop_mutation(
        &self,
        owner: (&str, u64),
        request: (&str, &cdr_app_server::RequestId),
        outcome: &str,
        claim: &Value,
    ) -> Result<(), AppServerError> {
        cdr_store::ingress::stop::control::finish_wire(
            &self.mirror_db,
            claim,
            (owner.0, i64::try_from(owner.1).map_err(mutation_held)?),
            (request.0, &serde_json::to_string(request.1)?),
            outcome,
        )
        .map_err(mutation_held)
    }

    fn finish_mutation(
        &self,
        owner: (&str, u64),
        request: (&str, &cdr_app_server::RequestId),
        outcome: &str,
    ) -> Result<(), AppServerError> {
        cdr_store::mutation_attempt::finish(
            &self.mirror_db,
            &cdr_store::mutation_attempt::Completion {
                runtime_id: &self.runtime_id,
                owner_id: owner.0,
                generation: i64::try_from(owner.1).map_err(mutation_held)?,
                attempt_id: request.0,
                wire_id: &serde_json::to_string(request.1)?,
                outcome,
            },
        )
        .map_err(mutation_held)
    }

    fn persist(&self, work: &DeadGenerationWork) -> Result<(), AppServerError> {
        let affected_targets = work
            .active_turns
            .iter()
            .map(|turn| turn.thread_id.clone())
            .chain(
                work.server_requests
                    .iter()
                    .filter_map(|request| extract_thread_id(&request.params)),
            )
            .collect::<Vec<_>>();
        let has_unscoped_requests = work
            .server_requests
            .iter()
            .any(|request| extract_thread_id(&request.params).is_none());
        let snapshot_json = serde_json::to_string(work)?;
        capture_dead_generation(
            &self.mirror_db,
            DeadGenerationCapture {
                runtime_id: &self.runtime_id,
                generation: i64::try_from(work.generation).map_err(failure)?,
                snapshot_json: &snapshot_json,
                affected_targets: &affected_targets,
                startup_channel_id: self.startup_channel_id,
                has_unscoped_requests,
                now: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(failure)?
                    .as_secs_f64(),
            },
        )
        .map_err(failure)?;
        Ok(())
    }

    fn check_request(
        &self,
        generation: u64,
        method: &str,
        params: &Value,
    ) -> Result<(), AppServerError> {
        if method == "thread/archive" {
            // Archive validates its own durable reservation at the actual write.
            return Ok(());
        }
        // Explicit interrupt and one-way server responses retain their existing
        // control/custody paths; neither clears an unresolved mutation record.
        if !cdr_app_server::requests::is_observational(method)
            && !matches!(method, "turn/interrupt" | "server/response")
        {
            if let Some(target) = extract_thread_id(params)
                && cdr_store::ingress::stop::control::target_is_held(&self.mirror_db, &target)
                    .map_err(mutation_held)?
            {
                return Err(mutation_held("stop execution end is not confirmed"));
            }
            cdr_store::mutation_attempt::check(
                &self.mirror_db,
                &self.runtime_id,
                extract_thread_id(params).as_deref(),
            )
            .map_err(mutation_held)?;
            if let Some(target) = extract_thread_id(params) {
                cdr_store::mutation_attempt::response::check(&self.mirror_db, &target)
                    .map_err(mutation_held)?;
            } else {
                cdr_store::mutation_attempt::response::check_all(&self.mirror_db)
                    .map_err(mutation_held)?;
            }
        }
        if !matches!(
            method,
            "thread/resume" | "thread/fork" | "turn/start" | "turn/steer"
        ) {
            return Ok(());
        }
        if let Some(target) = extract_thread_id(params)
            && target_is_held(&self.mirror_db, &target).map_err(failure)?
        {
            return Err(failure(cdr_store::StoreError::DeadGenerationTargetHeld(
                target,
            )));
        }
        if generation_is_sealed(&self.mirror_db, i64::try_from(generation).map_err(failure)?)
            .map_err(failure)?
        {
            return Err(failure("app-server generation is durably sealed"));
        }
        Ok(())
    }
}

fn failure(error: impl std::fmt::Display) -> AppServerError {
    AppServerError::DeadGenerationFence {
        message: error.to_string(),
    }
}

fn mutation_held(error: impl std::fmt::Display) -> AppServerError {
    AppServerError::MutationHeld {
        message: error.to_string(),
    }
}
