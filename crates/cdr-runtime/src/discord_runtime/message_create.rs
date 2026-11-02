use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use cdr_discord::http::DiscordHttp;
use cdr_store::StoreError;
use twilight_model::channel::Message;
use twilight_model::id::{Id, marker::ApplicationMarker};

use super::DiscordRuntimeError;
use super::typed_ingress::TypedIngressContext;
use crate::component_worker::pending_text_reply_available;
use crate::config::RuntimeConfig;
use crate::message_worker::{
    AdmittedMessage, ErrorReportTarget, MessageAdmissionError, MessageClassification,
    MessageWorkerError, admit_message_candidate_at, classify_gateway_message,
    process_admitted_gateway_message, process_with_error_report, report_processing_error,
};
use crate::restart_readiness::drain::{AdmissionGate, AdmissionPermit, DrainGateError};

mod receipt_echo;

enum PreparedMessage {
    Ignore(&'static str, u64, u64),
    Duplicate,
    Admitted(AdmittedMessage, Option<AdmissionPermit>),
    Unavailable,
}

enum MessageCreateBoundaryError {
    Store(StoreError),
    Admission(MessageAdmissionError),
    Drain(DrainGateError),
}

impl From<MessageCreateBoundaryError> for DiscordRuntimeError {
    fn from(error: MessageCreateBoundaryError) -> Self {
        match error {
            MessageCreateBoundaryError::Store(error) => Self::Store(error),
            MessageCreateBoundaryError::Admission(error) => Self::MessageAdmission(error),
            MessageCreateBoundaryError::Drain(error) => Self::RestartDrainGate(error),
        }
    }
}

fn prepare_message_create(
    message: Message,
    bot_user_id: Option<u64>,
    config: &RuntimeConfig,
    database: &Path,
    admission: &AdmissionGate,
    allow_drain_control: bool,
    resolver: &crate::settings_binding::SettingsTargetResolver,
) -> Result<PreparedMessage, MessageCreateBoundaryError> {
    prepare_message_create_with_routing(
        message,
        bot_user_id,
        config,
        database,
        SystemTime::now(),
        (admission, allow_drain_control, Some(resolver)),
    )
}

#[cfg(test)]
fn prepare_message_create_at(
    message: Message,
    bot_user_id: Option<u64>,
    config: &RuntimeConfig,
    database: &Path,
    observed_at: SystemTime,
) -> Result<PreparedMessage, MessageCreateBoundaryError> {
    prepare_message_create_at_with_gate(
        message,
        bot_user_id,
        config,
        database,
        observed_at,
        &AdmissionGate::new(),
        false,
    )
}

#[cfg(test)]
fn prepare_message_create_at_with_gate(
    message: Message,
    bot_user_id: Option<u64>,
    config: &RuntimeConfig,
    database: &Path,
    observed_at: SystemTime,
    admission: &AdmissionGate,
    allow_drain_control: bool,
) -> Result<PreparedMessage, MessageCreateBoundaryError> {
    prepare_message_create_with_routing(
        message,
        bot_user_id,
        config,
        database,
        observed_at,
        (admission, allow_drain_control, None),
    )
}

fn prepare_message_create_with_routing(
    message: Message,
    bot_user_id: Option<u64>,
    config: &RuntimeConfig,
    database: &Path,
    observed_at: SystemTime,
    routing: (
        &AdmissionGate,
        bool,
        Option<&crate::settings_binding::SettingsTargetResolver>,
    ),
) -> Result<PreparedMessage, MessageCreateBoundaryError> {
    let (admission, allow_drain_control, resolver) = routing;
    let policy = super::bootstrap::interaction_policy(config, database)
        .map_err(MessageCreateBoundaryError::Store)?;
    let candidate = match classify_gateway_message(message, database, config, &policy, bot_user_id)
        .map_err(MessageCreateBoundaryError::Admission)?
    {
        MessageClassification::Ignore(ignored) => {
            let (reason, channel_id, user_id) = ignored.into_log_parts();
            return Ok(PreparedMessage::Ignore(reason, channel_id, user_id));
        }
        MessageClassification::Candidate(candidate) => candidate,
    };
    let candidate = if let Some(resolver) = resolver {
        candidate
            .bind_settings(resolver)
            .map_err(MessageCreateBoundaryError::Admission)?
    } else {
        candidate
    };
    let pending_reply_only = allow_drain_control && candidate.is_pending_reply_candidate();
    let stop_control = candidate.is_stop_control();
    let force_restart = candidate.is_force_restart();
    if allow_drain_control && !pending_reply_only && !stop_control && !force_restart {
        return Ok(PreparedMessage::Unavailable);
    }
    // A force request is authenticated above, then durably deduplicated below.
    // It must remain executable even after normal AND control admission close.
    let permit = if force_restart {
        None
    } else {
        Some(
            match if pending_reply_only || stop_control {
                admission.try_enter_control()
            } else {
                admission.try_enter()
            } {
                Ok(permit) => permit,
                Err(DrainGateError::Sealed) => return Ok(PreparedMessage::Unavailable),
                Err(error) => return Err(MessageCreateBoundaryError::Drain(error)),
            },
        )
    };
    Ok(
        match admit_message_candidate_at(candidate, observed_at)
            .map_err(MessageCreateBoundaryError::Admission)?
        {
            Some(admitted) => PreparedMessage::Admitted(
                if pending_reply_only {
                    admitted
                        .require_pending_reply()
                        .map_err(MessageCreateBoundaryError::Admission)?
                } else {
                    admitted
                },
                permit,
            ),
            None => PreparedMessage::Duplicate,
        },
    )
}

async fn dispatch_message_create<Prepare, Process, ProcessFuture, Report, ReportFuture>(
    report_target: ErrorReportTarget,
    prepare: Prepare,
    process: Process,
    report: Report,
) -> Result<(), DiscordRuntimeError>
where
    Prepare: FnOnce() -> Result<PreparedMessage, MessageCreateBoundaryError>,
    Process: FnOnce(AdmittedMessage) -> ProcessFuture,
    ProcessFuture: Future<Output = Result<(), MessageWorkerError>>,
    Report: FnOnce(ErrorReportTarget, MessageWorkerError) -> ReportFuture,
    ReportFuture: Future<Output = ()>,
{
    let (admitted, admission_permit) = match prepare()? {
        PreparedMessage::Ignore(reason, channel_id, user_id) => {
            eprintln!("ignored_message reason={reason} chat={channel_id} user={user_id}");
            return Ok(());
        }
        PreparedMessage::Duplicate => return Ok(()),
        PreparedMessage::Unavailable => {
            report(report_target, MessageWorkerError::Restarting).await;
            return Ok(());
        }
        PreparedMessage::Admitted(admitted, permit) => (admitted, permit),
    };
    let admitted = admitted.retain_admission(admission_permit.as_ref());
    if let Err(error) = process_with_error_report(report_target, admitted, process, report).await {
        eprintln!("on_message_internal_admission_error: {error}");
        return Err(DiscordRuntimeError::MessageAdmission(error));
    }
    Ok(())
}

pub(super) async fn handle_message_create(
    message: Message,
    nonce: Option<u64>,
    bot_user_id: Option<u64>,
    application_id: Id<ApplicationMarker>,
    context: &TypedIngressContext,
) -> Result<(), DiscordRuntimeError> {
    let report_target = ErrorReportTarget::from_message(&message);
    let database = context.executor.mirror_db();
    if let Err(error) = receipt_echo::confirm(database, &message, nonce, bot_user_id) {
        eprintln!(
            "discord_gateway_receipt_confirmation_failed message_id={} error={error}",
            message.id
        );
    }
    let force_restart = cdr_discord::gateway::ingress::is_emergency_message(&message.content);
    let allow_drain_control = if !force_restart && context.admission.is_sealed() {
        match context.executor.target_thread_id(message.channel_id.get()) {
            Ok(target) => match pending_text_reply_available(&target, &context.server).await {
                Ok(available) => available,
                Err(error) => {
                    eprintln!("restart_drain_text_control_probe_failed: {error}");
                    false
                }
            },
            Err(error) => {
                eprintln!("restart_drain_text_control_target_failed: {error}");
                false
            }
        }
    } else {
        false
    };
    let message_context = context.message_context(application_id);
    let api = DiscordHttp::new(Arc::clone(&context.http), application_id)
        .with_store(context.executor.mirror_db(), Some(message.channel_id.get()));
    dispatch_message_create(
        report_target,
        || {
            prepare_message_create(
                message,
                bot_user_id,
                &context.config,
                database,
                &context.admission,
                allow_drain_control,
                &context.executor.settings_resolver(),
            )
        },
        |admitted| process_admitted_gateway_message(admitted, &message_context),
        |target, error| report_processing_error(database, &api, target, error),
    )
    .await
}

#[cfg(test)]
#[path = "message_create_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "message_create_drain_tests.rs"]
mod drain_tests;

#[cfg(test)]
#[path = "message_create_failure_tests.rs"]
mod failure_tests;

#[cfg(test)]
#[path = "message_create_settings_tests.rs"]
mod settings_tests;

#[cfg(test)]
#[path = "message_create_abandonment_tests.rs"]
mod abandonment_tests;
