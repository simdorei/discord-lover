use cdr_app_server::{
    AppServerError, RequestId, ResidentAppServer, ServerRequest, ServerRequestOccurrence,
    build_approval_response, build_input_response, extract_thread_id,
};
use cdr_discord::components::{
    ApprovalAnswer, ComponentId, ComponentRequestId, request_fingerprint, thread_fingerprint,
};
use serde_json::Value;

use super::ComponentWorkerError;

mod submit;
mod text_reply;

pub(super) use submit::submit_component_response;
pub use text_reply::handle_pending_text_reply;
pub(crate) use text_reply::pending_text_reply_available;

#[derive(Clone, Debug, PartialEq)]
pub struct ComponentResponse {
    pub request_id: RequestId,
    pub occurrence: ServerRequestOccurrence,
    pub payload: Value,
    pub confirmation: String,
    pub generation: u64,
}

pub fn build_component_response(
    component: &ComponentId,
    requests: &[ServerRequest],
    generation: u64,
) -> Result<ComponentResponse, ComponentWorkerError> {
    match component {
        ComponentId::AsyncChoice { .. }
        | ComponentId::AsyncTextInput { .. }
        | ComponentId::AsyncTextAnswer { .. }
        | ComponentId::RecoveryPublicationDecision { .. }
        | ComponentId::RecoveryAbandonDecision { .. } => {
            Err(ComponentWorkerError::InvalidComponent)
        }
        ComponentId::Approval { thread_id, answer } => {
            let request = legacy_request(requests, thread_id, |request| {
                is_approval_method(&request.method, &request.params)
            })?;
            approval_response(request, *answer, generation)
        }
        ComponentId::Input { thread_id, value } => {
            let request = legacy_request(requests, thread_id, |request| {
                request.method == "item/tool/requestUserInput"
            })?;
            input_response(request, value, generation)
        }
        ComponentId::BoundApproval {
            thread_fingerprint,
            request_fingerprint,
            answer,
        } => {
            let request = bound_request(
                requests,
                thread_fingerprint,
                request_fingerprint,
                generation,
            )?;
            if !is_approval_method(&request.method, &request.params) {
                return Err(ComponentWorkerError::NoPendingRequest);
            }
            approval_response(request, *answer, generation)
        }
        ComponentId::BoundInput {
            thread_fingerprint,
            request_fingerprint,
            value,
        } => {
            let request = bound_request(
                requests,
                thread_fingerprint,
                request_fingerprint,
                generation,
            )?;
            if request.method != "item/tool/requestUserInput" {
                return Err(ComponentWorkerError::NoPendingRequest);
            }
            input_response(request, value, generation)
        }
        ComponentId::Busy { .. } => Err(ComponentWorkerError::BusyChoice),
    }
}

pub(super) async fn prepare_component_response(
    component: &ComponentId,
    server: &ResidentAppServer,
) -> Result<ComponentResponse, ComponentWorkerError> {
    if matches!(
        component,
        ComponentId::RecoveryPublicationDecision { .. }
            | ComponentId::RecoveryAbandonDecision { .. }
    ) {
        return Err(ComponentWorkerError::InvalidComponent);
    }
    if matches!(component, ComponentId::Busy { .. }) {
        return Err(ComponentWorkerError::BusyChoice);
    }
    let generation = server.generation();
    let requests = server.pending_server_requests(None).await?;
    let actual = server.generation();
    if actual != generation {
        return Err(AppServerError::GenerationMismatch {
            expected: generation,
            actual,
        }
        .into());
    }
    build_component_response(component, &requests, generation)
}

fn legacy_request<'a>(
    requests: &'a [ServerRequest],
    thread_id: &str,
    matches_kind: impl Fn(&ServerRequest) -> bool,
) -> Result<&'a ServerRequest, ComponentWorkerError> {
    exactly_one(requests.iter().filter(|request| {
        extract_thread_id(&request.params).as_deref() == Some(thread_id) && matches_kind(request)
    }))
}

fn bound_request<'a>(
    requests: &'a [ServerRequest],
    expected_thread: &str,
    expected_request: &str,
    generation: u64,
) -> Result<&'a ServerRequest, ComponentWorkerError> {
    exactly_one(requests.iter().filter(|request| {
        extract_thread_id(&request.params).is_some_and(|thread_id| {
            thread_fingerprint(&thread_id).is_ok_and(|actual| actual == expected_thread)
                && request_fingerprint(
                    generation,
                    request.occurrence.as_bytes(),
                    component_request_id(&request.id),
                ) == expected_request
        })
    }))
}

fn exactly_one<'a>(
    mut matches: impl Iterator<Item = &'a ServerRequest>,
) -> Result<&'a ServerRequest, ComponentWorkerError> {
    let first = matches
        .next()
        .ok_or(ComponentWorkerError::NoPendingRequest)?;
    if matches.next().is_some() {
        return Err(ComponentWorkerError::AmbiguousPendingRequest);
    }
    Ok(first)
}

fn approval_response(
    request: &ServerRequest,
    answer: ApprovalAnswer,
    generation: u64,
) -> Result<ComponentResponse, ComponentWorkerError> {
    let (payload, action) =
        build_approval_response(&request.method, &request.params, approval_value(answer))?;
    Ok(ComponentResponse {
        request_id: request.id.clone(),
        occurrence: request.occurrence,
        payload,
        confirmation: format!("Approval response submitted: {action}"),
        generation,
    })
}

fn input_response(
    request: &ServerRequest,
    value: &str,
    generation: u64,
) -> Result<ComponentResponse, ComponentWorkerError> {
    Ok(ComponentResponse {
        request_id: request.id.clone(),
        occurrence: request.occurrence,
        payload: build_input_response(&request.params, value)?.payload,
        confirmation: "Codex input choice submitted.".into(),
        generation,
    })
}

fn component_request_id(request_id: &RequestId) -> ComponentRequestId<'_> {
    match request_id {
        RequestId::String(value) => ComponentRequestId::String(value),
        RequestId::Integer(value) => ComponentRequestId::Integer(*value),
    }
}

const fn approval_value(answer: ApprovalAnswer) -> &'static str {
    match answer {
        ApprovalAnswer::Approve => "1",
        ApprovalAnswer::ApproveSession => "2",
        ApprovalAnswer::Reject => "3",
        ApprovalAnswer::Cancel => "cancel",
    }
}

pub(crate) fn is_approval_method(method: &str, params: &Value) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "execCommandApproval"
            | "applyPatchApproval"
    ) || (method == "mcpServer/elicitation/request"
        && params.get("mode").and_then(Value::as_str) == Some("url"))
        || crate::server_request_worker::local_tools::requires_user_approval(method, params)
}
