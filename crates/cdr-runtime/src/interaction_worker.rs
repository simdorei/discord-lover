use std::sync::Arc;

use cdr_app_server::ResidentAppServer;
use cdr_discord::components::ComponentError;
use cdr_discord::delivery::DeliveryFailure;
use cdr_discord::http::{DiscordHttp, DiscordHttpError};
use cdr_discord::interaction::RoutedWork;
use thiserror::Error;
use tokio::sync::mpsc;
use twilight_http::Client;

use crate::action_executor::{ActionContext, ActionError, ActionExecutor};
use crate::command_plan::{CommandPlanError, plan_slash};
use crate::component_worker::{ComponentWorkerError, handle_component_work};
use crate::discord_dispatch::InboundInteractionWork;
use crate::discord_dispatch::delivery_identity::INTERACTION_FOLLOWUP_DOMAIN;
use crate::queue_runner::TurnBackend;

mod action_delivery;
mod async_answer_refusal;
#[cfg(test)]
mod busy_ingress_tests;
mod cleanup_refusal;
#[cfg(test)]
mod cleanup_refusal_tests;
mod custody;
mod delivery;
pub mod error_disposition;
mod error_report;
mod new_reply;

#[cfg(test)]
#[path = "interaction_worker/connected_tests.rs"]
mod connected_tests;
#[cfg(test)]
mod error_receipt_tests;

use custody::ExecutionCustody;
use error_report::report_interaction_error;

#[derive(Debug, Error)]
pub enum InteractionWorkerError {
    #[error("answer was not sent; notification remains unconfirmed: {0}")]
    AsyncAnswerNotice(String),
    #[error(transparent)]
    KnownOutcomeNotification(#[from] crate::cleanup_refusal::NotificationFailure),
    #[error(transparent)]
    PromptDelivery(#[from] crate::server_prompt_delivery::PromptDeliveryError),
    #[error(transparent)]
    Plan(#[from] CommandPlanError),
    #[error(transparent)]
    Action(#[from] ActionError),
    #[error("Discord interaction delivery failed: {0:?}")]
    Delivery(DeliveryFailure<DiscordHttpError>),
    #[error("interaction work type is not implemented yet: {0}")]
    Unsupported(&'static str),
    #[error(transparent)]
    Component(#[from] ComponentWorkerError),
    #[error(transparent)]
    Ui(#[from] ComponentError),
    #[error("durable interaction custody failed: {0}")]
    Custody(#[from] cdr_store::StoreError),
}

async fn process_interaction_work<B: TurnBackend>(
    work: &InboundInteractionWork,
    executor: &ActionExecutor<B>,
    server: &ResidentAppServer,
    http: Arc<Client>,
    custody: &mut ExecutionCustody,
) -> Result<bool, InteractionWorkerError> {
    if let Some(reason) = custody.request_rejection(work)? {
        custody.record_result(&serde_json::json!({
            "kind":"request_rejected", "action_completed":false, "control_dispatched":false,
            "error":reason,
        }))?;
        let result = crate::action_executor::ActionResult {
            text: format!("ERROR: {reason}"),
            waits_for_final: false,
            ui: None,
        };
        action_delivery::deliver(work, &result, executor.mirror_db(), server, http).await?;
        return Ok(false);
    }
    let result = match &work.work {
        RoutedWork::Slash(invocation) => {
            let action = plan_slash(invocation)?;
            let result = executor
                .execute_with_ingress_context(
                    action,
                    ActionContext {
                        channel_id: work.channel_id.get(),
                        user_id: work.user_id.get(),
                        discord_message_id: Some(work.interaction_id.get()),
                        auto_queue_when_busy: false,
                    },
                    &work.custody_ingress_id,
                )
                .await;
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    let Some(refusal) = crate::cleanup_refusal::from_action_error(&error) else {
                        return Err(error.into());
                    };
                    return cleanup_refusal::deliver(
                        work,
                        custody,
                        &DiscordHttp::new(http, work.application_id),
                        &refusal,
                    )
                    .await;
                }
            };
            custody.record_result(&serde_json::json!({
                "kind": "slash",
                "action_completed": true,
                "waits_for_final": result.waits_for_final,
                "response": result.text,
            }))?;
            result
        }
        RoutedWork::Autocomplete(_) => {
            return Err(InteractionWorkerError::Unsupported("autocomplete"));
        }
        RoutedWork::Component(component) => {
            let confirmation = match handle_component_work(work, component, executor, server).await
            {
                Ok(confirmation) => confirmation,
                Err(ComponentWorkerError::AsyncAnswerNotDispatched(refusal)) => {
                    return async_answer_refusal::deliver(work, custody, &http, &refusal).await;
                }
                Err(
                    error @ ComponentWorkerError::Busy(
                        crate::component_worker::BusyComponentError::ControlNotDispatched(_),
                    ),
                ) => {
                    custody.record_result(&serde_json::json!({
                        "kind": "busy_control_preflight_rejected",
                        "control_dispatched": false,
                        "error": error.to_string(),
                    }))?;
                    return Err(error.into());
                }
                Err(error) if error.action_completed_before_failure() => {
                    custody.record_result(&serde_json::json!({
                        "kind": "component",
                        "action_completed": true,
                        "confirmation_error": error.to_string(),
                    }))?;
                    return Err(error.into());
                }
                Err(error) => return Err(error.into()),
            };
            custody.record_result(&serde_json::json!({
                "kind": "component",
                "action_completed": true,
            }))?;
            confirmation.deliver(http).await?;
            return Ok(false);
        }
    };
    action_delivery::deliver(work, &result, executor.mirror_db(), server, http).await?;
    Ok(result.waits_for_final)
}

pub async fn run_interaction_worker<B: TurnBackend>(
    mut receiver: mpsc::Receiver<InboundInteractionWork>,
    executor: Arc<ActionExecutor<B>>,
    server: Arc<ResidentAppServer>,
    http: Arc<Client>,
) {
    while let Some(work) = receiver.recv().await {
        let mut custody = match ExecutionCustody::begin(
            executor.mirror_db(),
            &work.custody_database,
            &work.custody_ingress_id,
            work.processing_mode,
        ) {
            Ok(custody) => custody,
            Err(error) => {
                report_interaction_error(
                    &work,
                    InteractionWorkerError::Custody(error),
                    Arc::clone(&http),
                )
                .await;
                continue;
            }
        };
        let result =
            process_interaction_work(&work, &executor, &server, Arc::clone(&http), &mut custody)
                .await;
        match result {
            Ok(waits_for_final) => {
                if let Err(error) = custody.finish_notification(&serde_json::json!({
                    "kind": "interaction",
                    "action_completed": true,
                    "waits_for_final": waits_for_final,
                })) {
                    report_interaction_error(&work, error, Arc::clone(&http)).await;
                } else {
                    executor.notify_delivery_ready();
                }
            }
            Err(error) => {
                if let Err(hold_error) = custody.hold_failed() {
                    eprintln!(
                        "interaction_processing_hold_failed ingress={} error={hold_error}",
                        work.custody_ingress_id
                    );
                }
                report_interaction_error(&work, error, Arc::clone(&http)).await;
            }
        }
    }
}
