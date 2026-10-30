use serde::Serialize;
use thiserror::Error;
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseType};

mod async_choice;
mod claims;
mod fingerprint;
mod recovery_abandonment;
mod recovery_publication;
mod rows;

pub use async_choice::{async_answer_rows, async_choice_rows};
pub use claims::{persistent_claim_key, persistent_component_claim_key};
pub use fingerprint::{ComponentRequestId, request_fingerprint, thread_fingerprint};
pub use recovery_abandonment::{AbandonDecision, abandonment_decision_rows};
pub use recovery_publication::{PublicationDecision, publication_decision_rows};
pub use rows::{
    approval_button_row, bound_approval_button_row, bound_input_button_row, busy_button_row,
    input_button_row,
};

const MAX_CUSTOM_ID_CHARS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BusyAction {
    Steer,
    Queue,
    Stop,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ApprovalAnswer {
    Approve,
    ApproveSession,
    Reject,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ComponentId {
    AsyncTextInput {
        question_id: String,
    },
    AsyncTextAnswer {
        question_id: String,
        text: String,
    },
    RecoveryAbandonDecision {
        proposal_id: String,
        revision: i64,
        decision: AbandonDecision,
    },
    RecoveryPublicationDecision {
        proposal_id: String,
        revision: i64,
        decision: PublicationDecision,
    },
    AsyncChoice {
        question_id: String,
        option: usize,
    },
    Busy {
        choice_id: String,
        action: BusyAction,
    },
    Approval {
        thread_id: String,
        answer: ApprovalAnswer,
    },
    BoundApproval {
        thread_fingerprint: String,
        request_fingerprint: String,
        answer: ApprovalAnswer,
    },
    Input {
        thread_id: String,
        value: String,
    },
    BoundInput {
        thread_fingerprint: String,
        request_fingerprint: String,
        value: String,
    },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ComponentError {
    #[error("Discord component custom ID exceeds 100 characters")]
    TooLong,
    #[error("Discord component value is invalid")]
    Invalid,
}

#[must_use]
pub fn parse_component_id(custom_id: &str) -> Option<ComponentId> {
    if custom_id.chars().count() > MAX_CUSTOM_ID_CHARS {
        return None;
    }
    let parts = custom_id.split(':').collect::<Vec<_>>();
    match parts.as_slice() {
        ["codex_async_other", question] if valid_fingerprint(question, 64) => {
            Some(ComponentId::AsyncTextInput {
                question_id: (*question).into(),
            })
        }
        ["codex_discard", "v1", proposal, revision, decision] => {
            recovery_abandonment::parse(proposal, revision, decision)
        }
        ["codex_pub", "v1", proposal, revision, decision] => {
            recovery_publication::parse(proposal, revision, decision)
        }
        ["codex_async", question, option] if valid_fingerprint(question, 64) => {
            let index = option.parse::<usize>().ok()?;
            (index < 25 && index.to_string() == *option).then(|| ComponentId::AsyncChoice {
                question_id: (*question).into(),
                option: index,
            })
        }
        ["codex_busy", choice_id, action] if valid_choice_id(choice_id) => {
            parse_busy_action(action).map(|action| ComponentId::Busy {
                choice_id: (*choice_id).into(),
                action,
            })
        }
        ["codex_approval", thread_id, answer] if !thread_id.trim().is_empty() => {
            parse_approval(answer).map(|answer| ComponentId::Approval {
                thread_id: thread_id.trim().into(),
                answer,
            })
        }
        ["codex_approval", "v2", thread, request, answer]
            if valid_fingerprint(thread, 16) && valid_fingerprint(request, 32) =>
        {
            parse_bound_approval(answer).map(|answer| ComponentId::BoundApproval {
                thread_fingerprint: (*thread).into(),
                request_fingerprint: (*request).into(),
                answer,
            })
        }
        ["codex_input", thread_id, value]
            if !thread_id.trim().is_empty() && safe_input_value(value.trim()) =>
        {
            Some(ComponentId::Input {
                thread_id: thread_id.trim().into(),
                value: value.trim().into(),
            })
        }
        ["codex_input", "v2", thread, request, value]
            if valid_fingerprint(thread, 16)
                && valid_fingerprint(request, 32)
                && safe_input_value(value) =>
        {
            Some(ComponentId::BoundInput {
                thread_fingerprint: (*thread).into(),
                request_fingerprint: (*request).into(),
                value: (*value).into(),
            })
        }
        _ => None,
    }
}

pub fn format_input_choice(thread_id: &str, value: &str) -> Result<String, ComponentError> {
    let value = value.trim();
    if thread_id.trim().is_empty() || !safe_input_value(value) {
        return Err(ComponentError::Invalid);
    }
    bounded(format!("codex_input:{}:{value}", thread_id.trim()))
}

#[must_use]
pub const fn deferred_update() -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::DeferredUpdateMessage,
        data: None,
    }
}

fn bounded(value: String) -> Result<String, ComponentError> {
    if value.chars().count() <= MAX_CUSTOM_ID_CHARS {
        Ok(value)
    } else {
        Err(ComponentError::TooLong)
    }
}

fn valid_choice_id(value: &str) -> bool {
    value.len() == 24
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_fingerprint(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn safe_input_value(value: &str) -> bool {
    (1..=20).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

fn parse_busy_action(value: &str) -> Option<BusyAction> {
    match value.trim() {
        "steer" => Some(BusyAction::Steer),
        "queue" => Some(BusyAction::Queue),
        "stop" => Some(BusyAction::Stop),
        "ignore" => Some(BusyAction::Ignore),
        _ => None,
    }
}

fn parse_approval(value: &str) -> Option<ApprovalAnswer> {
    match value.trim() {
        "1" => Some(ApprovalAnswer::Approve),
        "2" => Some(ApprovalAnswer::ApproveSession),
        "3" => Some(ApprovalAnswer::Reject),
        "cancel" => Some(ApprovalAnswer::Cancel),
        _ => None,
    }
}

fn parse_bound_approval(value: &str) -> Option<ApprovalAnswer> {
    match value {
        "1" => Some(ApprovalAnswer::Approve),
        "2" => Some(ApprovalAnswer::ApproveSession),
        "3" => Some(ApprovalAnswer::Reject),
        "cancel" => Some(ApprovalAnswer::Cancel),
        _ => None,
    }
}
