use cdr_discord::components::{
    ApprovalAnswer, BusyAction, ComponentId, persistent_component_claim_key,
};
use twilight_model::id::{
    Id,
    marker::{InteractionMarker, MessageMarker},
};

pub const INTERACTION_FOLLOWUP_DOMAIN: &str = "interaction/followup/v1";
pub const INTERACTION_ERROR_DOMAIN: &str = "interaction/error/v1";
pub const COMPONENT_CONFIRMATION_DOMAIN: &str = "component/confirmation/v1";
pub const BUSY_CONFIRMATION_DOMAIN: &str = "component/busy-confirmation/v1";
pub const COMPONENT_ERROR_DOMAIN: &str = "interaction/error-component/v1";

/// Stable logical key for follow-up chunks created for one interaction.
#[must_use]
pub fn interaction_delivery_key(interaction_id: Id<InteractionMarker>) -> String {
    format!("interaction:{}", interaction_id.get())
}

/// Stable logical key for a component confirmation or error report.
///
/// Length prefixes keep component and claim fields unambiguous even if a
/// future component format permits delimiter characters.
#[must_use]
pub fn component_delivery_key(
    interaction_id: Id<InteractionMarker>,
    source_message_id: Option<Id<MessageMarker>>,
    component: &ComponentId,
    claim_identity: Option<&str>,
) -> String {
    let component = component_identity(component);
    let mut key = String::from("v1;");
    if let Some(source_message_id) = source_message_id {
        push_field(
            &mut key,
            "source-some",
            &source_message_id.get().to_string(),
        );
    } else {
        key.push_str("source-none;");
    }
    push_field(&mut key, "interaction", &interaction_id.get().to_string());
    push_field(&mut key, "component", &component);
    if let Some(claim_identity) = claim_identity {
        push_field(&mut key, "claim-some", claim_identity);
    } else {
        key.push_str("claim-none;");
    }
    key
}

#[must_use]
pub fn component_claim_identity(
    source_message_id: Option<Id<MessageMarker>>,
    component: &ComponentId,
) -> Option<String> {
    match component {
        ComponentId::RecoveryPublicationDecision { .. }
        | ComponentId::RecoveryAbandonDecision { .. } => None,
        ComponentId::AsyncChoice { question_id, .. }
        | ComponentId::AsyncTextInput { question_id }
        | ComponentId::AsyncTextAnswer { question_id, .. } => Some(question_id.clone()),
        ComponentId::Busy { choice_id, .. } => Some(choice_id.clone()),
        ComponentId::Approval { .. }
        | ComponentId::BoundApproval { .. }
        | ComponentId::Input { .. }
        | ComponentId::BoundInput { .. } => source_message_id
            .and_then(|message_id| persistent_component_claim_key(message_id.get(), component)),
    }
}

fn component_identity(component: &ComponentId) -> String {
    match component {
        ComponentId::AsyncTextInput { question_id } => format!("async-input:{question_id}"),
        ComponentId::AsyncTextAnswer { question_id, text } => {
            let mut identity = format!("async-text:{question_id};");
            push_field(&mut identity, "text", text);
            identity
        }
        ComponentId::RecoveryAbandonDecision {
            proposal_id,
            revision,
            decision,
        } => {
            let mut identity = String::from("abandonment-decision-v1;");
            push_field(&mut identity, "proposal", proposal_id);
            push_field(&mut identity, "revision", &revision.to_string());
            push_field(&mut identity, "decision", &format!("{decision:?}"));
            identity
        }
        ComponentId::RecoveryPublicationDecision {
            proposal_id,
            revision,
            decision,
        } => {
            let mut identity = String::from("publication-intent-v1;");
            push_field(&mut identity, "proposal", proposal_id);
            push_field(&mut identity, "revision", &revision.to_string());
            push_field(&mut identity, "decision", &format!("{decision:?}"));
            identity
        }
        ComponentId::AsyncChoice {
            question_id,
            option,
        } => format!("async-question:{question_id}:{option}"),
        ComponentId::Approval { thread_id, answer } => {
            let mut identity = String::from("approval;");
            push_field(&mut identity, "thread", thread_id);
            push_field(&mut identity, "answer", approval_code(*answer));
            identity
        }
        ComponentId::Input { thread_id, value } => {
            let mut identity = String::from("input;");
            push_field(&mut identity, "thread", thread_id);
            push_field(&mut identity, "value", value);
            identity
        }
        ComponentId::BoundApproval {
            thread_fingerprint,
            request_fingerprint,
            answer,
        } => {
            let mut identity = String::from("approval-v2;");
            push_field(&mut identity, "thread", thread_fingerprint);
            push_field(&mut identity, "request", request_fingerprint);
            push_field(&mut identity, "answer", approval_code(*answer));
            identity
        }
        ComponentId::BoundInput {
            thread_fingerprint,
            request_fingerprint,
            value,
        } => {
            let mut identity = String::from("input-v2;");
            push_field(&mut identity, "thread", thread_fingerprint);
            push_field(&mut identity, "request", request_fingerprint);
            push_field(&mut identity, "value", value);
            identity
        }
        ComponentId::Busy { choice_id, action } => {
            let mut identity = String::from("busy;");
            push_field(&mut identity, "choice", choice_id);
            push_field(&mut identity, "action", busy_code(*action));
            identity
        }
    }
}

fn push_field(output: &mut String, name: &str, value: &str) {
    output.push_str(name);
    output.push('=');
    output.push_str(&value.len().to_string());
    output.push(':');
    output.push_str(value);
    output.push(';');
}

const fn approval_code(answer: ApprovalAnswer) -> &'static str {
    match answer {
        ApprovalAnswer::Approve => "1",
        ApprovalAnswer::ApproveSession => "2",
        ApprovalAnswer::Reject => "3",
        ApprovalAnswer::Cancel => "cancel",
    }
}

const fn busy_code(action: BusyAction) -> &'static str {
    match action {
        BusyAction::Steer => "steer",
        BusyAction::Queue => "queue",
        BusyAction::Stop => "stop",
        BusyAction::Ignore => "ignore",
    }
}
