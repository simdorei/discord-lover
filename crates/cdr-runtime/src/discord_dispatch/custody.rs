//! Durable, token-free custody acquired before a Discord interaction ACK.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use cdr_discord::components::{BusyAction, ComponentId};
use cdr_discord::interaction::RoutedWork;
use cdr_store::StoreError;
use cdr_store::claims::BusyChoice;
use cdr_store::ingress::{IngressAdmission, IngressKind, NewIngress};

pub(super) enum StageOutcome {
    Created(StagedCustody),
    Duplicate,
    CanonicalRepeat(CanonicalRepeat),
    BusyChoiceUnavailable,
}

pub(super) struct CanonicalRepeat {
    pub receipt: CustodyReceipt,
    pub confirmation_ready: bool,
}

pub(super) struct StagedCustody {
    database: PathBuf,
    ingress_id: String,
    busy_choice: Option<BusyChoice>,
    armed: bool,
}

pub(super) struct CustodyReceipt {
    pub database: PathBuf,
    pub ingress_id: String,
    pub busy_choice: Option<BusyChoice>,
}

pub(super) struct StageRequest<'a> {
    pub application_id: u64,
    pub interaction_id: u64,
    pub channel_id: u64,
    pub user_id: u64,
    pub source_message_id: Option<u64>,
    pub work: &'a RoutedWork,
    pub settings_resolver: Option<&'a crate::settings_binding::SettingsTargetResolver>,
}

pub(super) fn stage(
    database: &Path,
    request: &StageRequest<'_>,
) -> Result<StageOutcome, StoreError> {
    let interaction_id = to_i64(request.interaction_id, "interaction")?;
    let channel_id = to_i64(request.channel_id, "channel")?;
    let owner_user_id = to_i64(request.user_id, "user")?;
    let source_message_id = request
        .source_message_id
        .map(|id| to_i64(id, "source message"))
        .transpose()?;
    let ingress_id = format!("interaction:{interaction_id}");
    let settings = super::settings_admission::prepare(
        request.work,
        request.settings_resolver,
        request.channel_id,
    )?;
    let (canonical_owner, busy) = match request.work {
        RoutedWork::Component(ComponentId::Busy { choice_id, action }) => (
            format!("busy-choice:{choice_id}"),
            Some((choice_id.as_str(), action_name(*action))),
        ),
        _ => (ingress_id.clone(), None),
    };
    let new_ingress = NewIngress {
        ingress_id,
        kind: IngressKind::Interaction,
        event_id: Some(interaction_id),
        application_id: Some(to_i64(request.application_id, "application")?),
        channel_id,
        owner_user_id,
        source_message_id,
        payload: serde_json::json!({
            "version": 1,
            "processing_mode": "normal",
            "work": request.work,
            "settings_binding":settings.binding,
            "request_rejection":settings.rejection,
        }),
        target_thread_id: abandonment_target(database, request)?
            .or(publication_target(database, request)?)
            .or(super::async_modal::answer_target(database, request)?)
            .or_else(|| {
                settings
                    .binding
                    .as_ref()
                    .map(|binding| binding.target.clone())
            }),
        canonical_owner: Some(canonical_owner),
        now: now()?,
    };
    let admission = match busy {
        Some((choice_id, action)) => {
            match cdr_store::ingress::admit_busy_interaction(
                database,
                &new_ingress,
                choice_id,
                action,
            ) {
                Ok(admission) => admission,
                Err(StoreError::BusyChoiceUnavailable(_)) => {
                    return Ok(StageOutcome::BusyChoiceUnavailable);
                }
                Err(error) => return Err(error),
            }
        }
        None => match request.work {
            RoutedWork::Slash(invocation)
                if matches!(invocation.name.as_str(), "ask" | "interview") =>
            {
                cdr_store::ingress::admit_mapped_slash_prompt(database, &new_ingress)?
            }
            _ => cdr_store::ingress::admit(database, &new_ingress)?,
        },
    };
    from_admission(database, admission, busy.is_some())
}

fn publication_target(
    database: &Path,
    request: &StageRequest<'_>,
) -> Result<Option<String>, StoreError> {
    let RoutedWork::Component(ComponentId::RecoveryPublicationDecision {
        proposal_id,
        revision,
        ..
    }) = request.work
    else {
        return Ok(None);
    };
    let binding = cdr_store::async_resolution::publication::delivered_proposal(
        database,
        proposal_id,
        *revision,
    )?;
    binding.require_actor(
        to_i64(request.application_id, "application")?,
        to_i64(request.channel_id, "channel")?,
        to_i64(request.user_id, "user")?,
        to_i64(
            request.source_message_id.ok_or_else(|| {
                StoreError::Integrity("publication component has no source message".into())
            })?,
            "source message",
        )?,
    )?;
    Ok(Some(binding.proposal.thread_id))
}

fn abandonment_target(
    database: &Path,
    request: &StageRequest<'_>,
) -> Result<Option<String>, StoreError> {
    use cdr_discord::components::AbandonDecision;
    use cdr_store::async_resolution::abandonment as store;
    let RoutedWork::Component(ComponentId::RecoveryAbandonDecision {
        proposal_id,
        revision,
        decision,
    }) = request.work
    else {
        return Ok(None);
    };
    let decision = match decision {
        AbandonDecision::AbandonOnly => store::Decision::AbandonOnly,
        AbandonDecision::KeepHeld => store::Decision::KeepHeld,
    };
    let binding = store::authorize_decision(
        database,
        &store::DecisionRouteInput {
            proposal_id,
            revision: *revision,
            decision,
            interaction_id: to_i64(request.interaction_id, "interaction")?,
            application_id: to_i64(request.application_id, "application")?,
            channel_id: to_i64(request.channel_id, "channel")?,
            owner_user_id: to_i64(request.user_id, "user")?,
            source_message_id: to_i64(
                request.source_message_id.ok_or_else(|| {
                    StoreError::Integrity("abandonment component has no source message".into())
                })?,
                "source message",
            )?,
            now: now()?,
        },
    )?;
    Ok(Some(binding.proposal.thread_id))
}

fn from_admission(
    database: &Path,
    admission: IngressAdmission,
    busy: bool,
) -> Result<StageOutcome, StoreError> {
    if !admission.created {
        if admission.canonical_repeat_created {
            if !busy {
                return Err(StoreError::Integrity(
                    "non-busy interaction unexpectedly coalesced by canonical owner".into(),
                ));
            }
            let record = admission.record.ok_or_else(|| {
                StoreError::Integrity("canonical interaction repeat has no durable record".into())
            })?;
            let confirmation_ready = match record.owner_kind.as_deref() {
                Some("prompt") => true,
                Some("ingress") => false,
                _ => {
                    return Err(StoreError::Integrity(
                        "canonical interaction repeat has no durable owner".into(),
                    ));
                }
            };
            let busy_choice = admission.busy_choice.ok_or_else(|| {
                StoreError::Integrity(
                    "canonical interaction repeat has no frozen authorization snapshot".into(),
                )
            })?;
            return Ok(StageOutcome::CanonicalRepeat(CanonicalRepeat {
                receipt: CustodyReceipt {
                    database: database.to_owned(),
                    ingress_id: record.ingress_id,
                    busy_choice: Some(busy_choice),
                },
                confirmation_ready,
            }));
        }
        return Ok(StageOutcome::Duplicate);
    }
    let record = admission.record.ok_or_else(|| {
        StoreError::Integrity("new interaction custody has no durable record".into())
    })?;
    if busy != admission.busy_choice.is_some() {
        return Err(StoreError::Integrity(
            "busy interaction custody has no frozen authorization snapshot".into(),
        ));
    }
    Ok(StageOutcome::Created(StagedCustody {
        database: database.to_owned(),
        ingress_id: record.ingress_id,
        busy_choice: admission.busy_choice,
        armed: true,
    }))
}

impl StagedCustody {
    pub fn acknowledge(&mut self) -> Result<(), StoreError> {
        if !cdr_store::ingress::acknowledge(&self.database, &self.ingress_id, now()?)? {
            return Err(StoreError::Integrity(
                "interaction custody acknowledgement is no longer current".into(),
            ));
        }
        Ok(())
    }

    pub fn hold_not_executed(&mut self, reason: &'static str) -> Result<(), StoreError> {
        cdr_store::ingress::hold(&self.database, &self.ingress_id, reason, true, now()?)?;
        self.armed = false;
        Ok(())
    }

    pub fn into_receipt(mut self) -> CustodyReceipt {
        self.armed = false;
        CustodyReceipt {
            database: std::mem::take(&mut self.database),
            ingress_id: std::mem::take(&mut self.ingress_id),
            busy_choice: self.busy_choice.take(),
        }
    }
}

impl Drop for StagedCustody {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let now = match now() {
            Ok(now) => now,
            Err(error) => {
                eprintln!("interaction_custody_cancel_hold_clock_failed: {error}");
                return;
            }
        };
        if let Err(error) = cdr_store::ingress::hold(
            &self.database,
            &self.ingress_id,
            "interaction_dispatch_cancelled",
            true,
            now,
        ) {
            eprintln!("interaction_custody_cancel_hold_failed: {error}");
        }
    }
}

fn action_name(action: BusyAction) -> &'static str {
    match action {
        BusyAction::Steer => "steer",
        BusyAction::Queue => "queue",
        BusyAction::Stop => "stop",
        BusyAction::Ignore => "ignore",
    }
}

fn to_i64(value: u64, kind: &str) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Integrity(format!("Discord {kind} ID exceeds SQLite range")))
}

pub(super) fn now() -> Result<f64, StoreError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64())
}
