use super::{ResidentAppServer, target_mutation};
use crate::{
    AppServerError, RequestId, RpcErrorPayload, ServerRequestOccurrence, requests::AppRequest,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

pub(super) type DispatchCheck = Arc<dyn Fn() -> Result<(), AppServerError> + Send + Sync>;

#[derive(Default)]
struct DispatchContext {
    check: Option<DispatchCheck>,
    queue_claim: Option<Value>,
    stop_claim: Option<Value>,
    client_pin: Option<crate::AppServerClient>,
}

mod mutation;
pub(super) mod origin;

impl ResidentAppServer {
    /// Archive's validated descendants share the root's original stop revision.
    /// This does not grant ordinary settings, starts, or child archive authority.
    pub async fn with_archive_stop_scope<T>(
        root: &str,
        children: &std::collections::BTreeSet<String>,
        operation: impl std::future::Future<Output = T>,
    ) -> Result<T, AppServerError> {
        match origin::archive(root, children)? {
            Some(frozen) => Ok(origin::scope(Some(frozen), operation).await),
            None => Ok(operation.await),
        }
    }

    /// Carry server-owned durable admission metadata through pre-RPC waits.
    /// Nesting preserves the first origin; it cannot refresh an older request.
    pub async fn with_stop_origin<T>(
        frozen: Option<Value>,
        operation: impl std::future::Future<Output = T>,
    ) -> T {
        if origin::is_set() {
            operation.await
        } else {
            origin::scope(frozen, operation).await
        }
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        wait: Duration,
        expected_generation: Option<u64>,
    ) -> Result<Value, AppServerError> {
        self.request_scoped(
            method,
            params,
            wait,
            expected_generation,
            false,
            DispatchContext::default(),
        )
        .await
    }

    /// Only for a repair whose caller holds the target queue lock through an
    /// indeterminate outcome. A fully flushed tool timeout must not fence B.
    /// Partial writes and transport failures retain the ordinary global fence.
    pub async fn request_for_tool_repair(
        &self,
        method: &str,
        params: Value,
        wait: Duration,
        generation: u64,
    ) -> Result<Value, AppServerError> {
        self.request_for_tool_repair_checked(method, params, wait, generation, Arc::new(|| Ok(())))
            .await
    }

    /// Recheck frozen command custody after waits, including the actual pipe write.
    pub async fn request_for_tool_repair_checked(
        &self,
        method: &str,
        params: Value,
        wait: Duration,
        generation: u64,
        check: Arc<dyn Fn() -> Result<(), AppServerError> + Send + Sync>,
    ) -> Result<Value, AppServerError> {
        let scoped = params
            .get("threadId")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty());
        let allowed = matches!(method, "thread/read" | "mcpServerStatus/list")
            || (method == "mcpServer/tool/call"
                && params["server"] == "node_repl"
                && matches!(params["tool"].as_str(), Some("js" | "js_reset")));
        if !scoped || !allowed {
            return Err(AppServerError::InvalidReply {
                message: "tool repair requires a scoped node_repl request".into(),
            });
        }
        self.request_scoped(
            method,
            params,
            wait,
            Some(generation),
            true,
            DispatchContext {
                check: Some(check),
                queue_claim: None,
                stop_claim: None,
                client_pin: None,
            },
        )
        .await
    }

    /// Only the native recovery collector supplies this private client pin.
    pub(super) async fn request_for_recovery_observation(
        &self,
        pinned: &super::admission::ResidentAdmission,
        request: AppRequest,
    ) -> Result<Value, AppServerError> {
        if !matches!(
            request.method,
            "thread/read" | "thread/turns/list" | "thread/goal/get"
        ) || request.params["threadId"]
            .as_str()
            .is_none_or(str::is_empty)
        {
            return Err(AppServerError::InvalidReply {
                message: "recovery observation only supports exact read-only requests".into(),
            });
        }
        let state = self.state.clone();
        let client = pinned.client.clone();
        let generation = pinned.generation;
        self.request_scoped(
            request.method,
            request.params,
            request.timeout,
            Some(generation),
            false,
            DispatchContext {
                check: Some(Arc::new(move || {
                    state.with_recovery_current(&client, generation, || ())
                })),
                queue_claim: None,
                stop_claim: None,
                client_pin: Some(pinned.client.clone()),
            },
        )
        .await
    }

    async fn request_scoped(
        &self,
        method: &str,
        params: Value,
        wait: Duration,
        expected_generation: Option<u64>,
        isolate: bool,
        context: DispatchContext,
    ) -> Result<Value, AppServerError> {
        if origin::is_set() {
            return self
                .request_scoped_inner(method, params, wait, expected_generation, isolate, context)
                .await;
        }
        let frozen = self
            .dead_generation_fence
            .as_ref()
            .map(|fence| fence.request_origin(method, &params))
            .transpose()?
            .flatten();
        origin::scope(
            frozen,
            self.request_scoped_inner(method, params, wait, expected_generation, isolate, context),
        )
        .await
    }

    async fn request_scoped_inner(
        &self,
        method: &str,
        params: Value,
        wait: Duration,
        expected_generation: Option<u64>,
        isolate: bool,
        context: DispatchContext,
    ) -> Result<Value, AppServerError> {
        let DispatchContext {
            check,
            queue_claim,
            stop_claim,
            client_pin,
        } = context;
        if let Some(check) = &check {
            check()?;
        }
        let admission = self.state.admit_request(expected_generation)?;
        require_client_pin(client_pin.as_ref(), &admission.client)?;
        let mutation = self
            .prepare_mutation_checked(method, &params, admission.generation, check.clone())
            .await?;
        let permit = match mutation {
            target_mutation::Prepared::Completed(value) => return Ok(value),
            target_mutation::Prepared::Ready(permit) => permit,
        };
        if let Some(fence) = &self.dead_generation_fence {
            fence.check_request(admission.generation, method, &params)?;
        }
        let mut written = self.state.track_written_request(admission.generation);
        let observational = crate::requests::is_observational(method);
        let attempt = mutation::Attempt::new(
            self,
            admission.generation,
            method,
            &params,
            isolate,
            queue_claim.as_ref(),
            stop_claim.as_ref(),
        );
        let flushed = written.isolate_after_flush();
        let result = admission
            .client
            .request_admitted_with_identity_checks(
                method,
                params.clone(),
                wait,
                |wire_id| {
                    if let Some(check) = &check {
                        check()?;
                    }
                    self.check_actual_mutation(
                        permit.as_ref(),
                        admission.generation,
                        method,
                        &params,
                    )?;
                    attempt.begin(wire_id)?;
                    if let Some(check) = &check {
                        check()?;
                    }
                    Ok(())
                },
                || {
                    attempt.write_started();
                    written.confirm_write_started();
                },
                || {
                    if isolate || observational || attempt.isolates_target() {
                        flushed.store(true, std::sync::atomic::Ordering::Release);
                    }
                },
            )
            .await;
        if matches!(result, Err(AppServerError::Timeout { .. }))
            && !observational
            && attempt.was_started()
            && !written.is_isolated()
        {
            self.state.mark_timeout(admission.generation);
        }
        written.finish(&result);
        attempt.finish(result).inspect_err(|error| {
            if matches!(error, AppServerError::MutationOutcomeUnknown { .. })
                && attempt.was_started()
                && !written.is_isolated()
            {
                self.state.mark_timeout(admission.generation);
            }
        })
    }

    pub async fn execute(
        &self,
        request: AppRequest,
        expected_generation: Option<u64>,
    ) -> Result<Value, AppServerError> {
        self.request_scoped(
            request.method,
            request.params,
            request.timeout,
            expected_generation,
            request.method == "thread/archive",
            DispatchContext::default(),
        )
        .await
    }

    /// Original queue authority is local metadata, never part of the RPC body.
    /// As with `execute()`, durable validation requires an installed fence; an
    /// installed legacy fence must explicitly support this claim contract.
    pub async fn execute_queue_turn(
        &self,
        request: AppRequest,
        generation: u64,
        claim: Value,
    ) -> Result<Value, AppServerError> {
        let target_matches = request
            .params
            .get("threadId")
            .and_then(Value::as_str)
            .is_some_and(|target| {
                !target.trim().is_empty()
                    && claim.get("target_thread_id").and_then(Value::as_str) == Some(target)
            });
        if request.method != "turn/start"
            || !target_matches
            || claim.get("app_server_generation").and_then(Value::as_u64) != Some(generation)
        {
            return Err(AppServerError::MutationHeld {
                message: "queue dispatch does not match its original claim".into(),
            });
        }
        self.request_scoped(
            request.method,
            request.params,
            request.timeout,
            Some(generation),
            false,
            DispatchContext {
                check: None,
                queue_claim: Some(claim),
                stop_claim: None,
                client_pin: None,
            },
        )
        .await
    }

    /// Bounded one-use interrupt; local custody metadata is not sent on the wire.
    pub async fn execute_stop_control(
        &self,
        request: AppRequest,
        generation: u64,
        claim: Value,
        check: DispatchCheck,
    ) -> Result<Value, AppServerError> {
        if self.dead_generation_fence.is_none()
            || request.method != "turn/interrupt"
            || claim["control"]["target"] != request.params["threadId"]
            || claim["control"]["turn"] != request.params["turnId"]
            || claim["control"]["generation"].as_u64() != Some(generation)
            || claim["control"]["resident"].as_str() != Some(self.instance_id())
        {
            return Err(AppServerError::MutationHeld {
                message: "stop dispatch does not match original durable authority".into(),
            });
        }
        self.request_scoped(
            request.method,
            request.params,
            request.timeout,
            Some(generation),
            false,
            DispatchContext {
                check: Some(check),
                queue_claim: None,
                stop_claim: Some(claim),
                client_pin: None,
            },
        )
        .await
    }

    pub async fn respond(
        &self,
        id: &RequestId,
        occurrence: ServerRequestOccurrence,
        result: Value,
        expected_generation: u64,
    ) -> Result<(), AppServerError> {
        let admission = self.state.admit_response(expected_generation)?;
        let permit = self
            .response_permit(&admission.client, id, occurrence, admission.generation)
            .await?;
        let attempt = super::target_mutation::ResponseAttempt::new(
            self,
            admission.generation,
            permit,
            crate::rpc::response_value(id, &result),
        );
        let mut written = self.state.track_written_request(admission.generation);
        let result = admission
            .client
            .respond_admitted_with_checks(
                id,
                occurrence,
                result,
                || attempt.begin(),
                || {
                    attempt.write_started();
                    written.confirm_write_started();
                },
            )
            .await;
        written.finish(&result);
        attempt.finish(result)
    }

    pub async fn respond_error(
        &self,
        id: &RequestId,
        occurrence: ServerRequestOccurrence,
        error: RpcErrorPayload,
        expected_generation: u64,
    ) -> Result<(), AppServerError> {
        let admission = self.state.admit_response(expected_generation)?;
        let permit = self
            .response_permit(&admission.client, id, occurrence, admission.generation)
            .await?;
        let attempt = super::target_mutation::ResponseAttempt::new(
            self,
            admission.generation,
            permit,
            crate::rpc::error_value(id, &error),
        );
        let mut written = self.state.track_written_request(admission.generation);
        let result = admission
            .client
            .respond_error_admitted_with_checks(
                id,
                occurrence,
                error,
                || attempt.begin(),
                || {
                    attempt.write_started();
                    written.confirm_write_started();
                },
            )
            .await;
        written.finish(&result);
        attempt.finish(result)
    }
}

fn require_client_pin(
    pinned: Option<&crate::AppServerClient>,
    actual: &crate::AppServerClient,
) -> Result<(), AppServerError> {
    if pinned.is_some_and(|expected| !Arc::ptr_eq(&expected.inner, &actual.inner)) {
        return Err(AppServerError::MutationHeld {
            message: "recovery observation admission changed client identity".into(),
        });
    }
    Ok(())
}
