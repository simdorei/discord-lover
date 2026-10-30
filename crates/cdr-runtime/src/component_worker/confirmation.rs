use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use cdr_discord::components::ComponentId;
use cdr_store::claims::claim_component;
use cdr_store::schema::open_initialized;
use cdr_store::{Result as StoreResult, StoreError};
use rusqlite::params;
use thiserror::Error;
use twilight_http::Client;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, MessageMarker},
};

use crate::discord_dispatch::delivery_identity::{
    BUSY_CONFIRMATION_DOMAIN, COMPONENT_CONFIRMATION_DOMAIN,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfirmationPlan {
    pub content: &'static str,
    pub domain: &'static str,
    pub logical_key: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfirmationClaimState {
    ExecuteAction,
    ActionUnconfirmed,
    DeliverConfirmation,
}

#[derive(Debug, Error)]
pub enum ConfirmationPlanError {
    #[error("busy components require a busy confirmation plan")]
    BusyComponent,
}

#[derive(Debug, Error)]
pub enum ConfirmationError {
    #[error("action succeeded; confirmation delivery failed and remains retryable: {0}")]
    Delivery(String),
    #[error("action succeeded; durable confirmation recovery state failed: {0}")]
    Recovery(String),
    #[error(
        "action and confirmation succeeded; clearing handled Discord buttons failed and remains \
         retryable: {0}"
    )]
    Clear(String),
}

#[must_use]
pub fn standard_ready_marker(action_claim: &str) -> String {
    ready_marker("standard", action_claim, None)
}

#[must_use]
pub fn busy_ready_marker(choice_id: &str, owner_user_id: u64, channel_id: u64) -> String {
    ready_marker(
        "busy",
        choice_id,
        Some((owner_user_id.to_string(), channel_id.to_string())),
    )
}

pub fn standard_confirmation_plan(
    component: &ComponentId,
    action_claim: &str,
) -> Result<ConfirmationPlan, ConfirmationPlanError> {
    let (kind, content) = match component {
        ComponentId::Approval { .. } | ComponentId::BoundApproval { .. } => {
            ("approval", "Approval response submitted.")
        }
        ComponentId::Input { .. } | ComponentId::BoundInput { .. } => {
            ("input", "Codex input choice submitted.")
        }
        ComponentId::Busy { .. }
        | ComponentId::AsyncTextInput { .. }
        | ComponentId::AsyncTextAnswer { .. }
        | ComponentId::AsyncChoice { .. }
        | ComponentId::RecoveryPublicationDecision { .. }
        | ComponentId::RecoveryAbandonDecision { .. } => {
            return Err(ConfirmationPlanError::BusyComponent);
        }
    };
    Ok(ConfirmationPlan {
        content,
        domain: COMPONENT_CONFIRMATION_DOMAIN,
        logical_key: delivery_key(kind, action_claim),
    })
}

#[must_use]
pub fn busy_confirmation_plan(choice_id: &str) -> ConfirmationPlan {
    ConfirmationPlan {
        content: "Busy action submitted.",
        domain: BUSY_CONFIRMATION_DOMAIN,
        logical_key: delivery_key("busy", choice_id),
    }
}

pub fn claim_standard_action(
    database: &Path,
    action_claim: &str,
    ready_marker: &str,
    now: f64,
    time_to_live: f64,
) -> StoreResult<ConfirmationClaimState> {
    if confirmation_ready(database, ready_marker, now)? {
        return Ok(ConfirmationClaimState::DeliverConfirmation);
    }
    if claim_component(database, action_claim, now, time_to_live)? {
        return Ok(ConfirmationClaimState::ExecuteAction);
    }
    if confirmation_ready(database, ready_marker, now)? {
        Ok(ConfirmationClaimState::DeliverConfirmation)
    } else {
        Ok(ConfirmationClaimState::ActionUnconfirmed)
    }
}

pub fn confirmation_ready(database: &Path, ready_marker: &str, now: f64) -> StoreResult<bool> {
    Ok(open_initialized(database)?.query_row(
        "SELECT EXISTS(SELECT 1 FROM persistent_component_claims \
         WHERE claim_key = ? AND expires_at > ?)",
        params![ready_marker, now],
        |row| row.get(0),
    )?)
}

pub fn record_confirmation_ready(
    database: &Path,
    ready_marker: &str,
    now: f64,
    time_to_live: f64,
) -> StoreResult<bool> {
    claim_component(database, ready_marker, now, time_to_live)
}

pub async fn deliver_confirmation_and_clear(
    http: Arc<Client>,
    database: &Path,
    channel_id: Id<ChannelMarker>,
    source_message_id: Option<Id<MessageMarker>>,
    plan: &ConfirmationPlan,
) -> Result<(), ConfirmationError> {
    send_then_clear(
        async {
            crate::completion_worker::send_recorded_message_with_components(
                database,
                &http,
                channel_id,
                &crate::completion_worker::IdempotentChunk {
                    domain: plan.domain,
                    logical_key: plan.logical_key.clone(),
                    chunk_index: 0,
                    content: plan.content.into(),
                },
                &[],
            )
            .await
            .map_err(|error| ConfirmationError::Delivery(error.to_string()))
        },
        async {
            if let Some(message_id) = source_message_id
                && let Err(error) = http
                    .update_message(channel_id, message_id)
                    .components(Some(&[]))
                    .await
            {
                return Err(ConfirmationError::Clear(error.to_string()));
            }
            Ok(())
        },
    )
    .await
}

pub async fn send_then_clear<SendFuture, ClearFuture, Error>(
    send: SendFuture,
    clear: ClearFuture,
) -> Result<(), Error>
where
    SendFuture: Future<Output = Result<(), Error>>,
    ClearFuture: Future<Output = Result<(), Error>>,
{
    send.await?;
    clear.await
}

#[must_use]
pub fn recovery_error(error: &StoreError) -> ConfirmationError {
    ConfirmationError::Recovery(error.to_string())
}

fn delivery_key(kind: &str, action_claim: &str) -> String {
    let mut key = String::from("v1;");
    push_field(&mut key, "kind", kind);
    push_field(&mut key, "action", action_claim);
    key
}

fn ready_marker(kind: &str, action_claim: &str, actor: Option<(String, String)>) -> String {
    let mut key = String::from("confirmation-ready:v1;");
    push_field(&mut key, "kind", kind);
    push_field(&mut key, "action", action_claim);
    if let Some((user_id, channel_id)) = actor {
        push_field(&mut key, "user", &user_id);
        push_field(&mut key, "channel", &channel_id);
    }
    key
}

fn push_field(output: &mut String, name: &str, value: &str) {
    output.push_str(name);
    output.push('=');
    output.push_str(&value.len().to_string());
    output.push(':');
    output.push_str(value);
    output.push(';');
}
