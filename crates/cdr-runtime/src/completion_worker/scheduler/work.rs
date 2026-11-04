use super::super::{
    CompletionWorker, CompletionWorkerError, IdempotentChunk, Processing, i64_channel,
};
use cdr_store::completion_work::{self, Entry, Payload, Source};

fn load(
    worker: &CompletionWorker,
    entry: &Entry,
) -> Result<Option<Payload>, CompletionWorkerError> {
    Ok(completion_work::load(
        worker.queue.db_path(),
        entry,
        worker.server.instance_id(),
        i64::try_from(worker.server.generation())
            .map_err(|_| crate::queue_runner::QueueRunnerError::IntegerRange)?,
    )?)
}

pub(super) fn prepare<'a>(
    worker: &'a CompletionWorker,
    item: &super::lanes::StateWork,
) -> Result<Option<(super::super::StateAdmission<'a>, bool)>, CompletionWorkerError> {
    let Some(lease) = worker.queue.try_target_lease(item.target())? else {
        return Ok(None);
    };
    let turn = match item {
        super::lanes::StateWork::Live(event) => match &event.event {
            cdr_app_server::ResidentNotificationEvent::Notification { notification, .. }
                if notification.method == "turn/completed" =>
            {
                cdr_app_server::extract_turn_id(&notification.params)
            }
            _ => None,
        },
        super::lanes::StateWork::Durable(entry) if entry.source == Source::Observed => {
            Some(entry.turn.clone())
        }
        super::lanes::StateWork::Durable(_) | super::lanes::StateWork::WriterRetry(_) => None,
    };
    let owner = if let Some(turn) = turn {
        cdr_store::queue::list_filtered(worker.queue.db_path(), Some(item.target()), None)?
            .into_iter()
            .find(|job| {
                job.state == cdr_store::queue::QueueJobState::Running
                    && job.turn_id.as_deref() == Some(turn.as_str())
            })
            .map_or(super::super::AdmissionOwner::Missing, |job| {
                super::super::AdmissionOwner::Exact(Box::new(job))
            })
    } else {
        super::super::AdmissionOwner::NotTerminal
    };
    // A failed/interrupted Goal terminal also needs history. Capture the exact
    // owner while holding the real lock; execution rejects a changed snapshot.
    let native = item.needs_native()
        || matches!(&owner,
        super::super::AdmissionOwner::Exact(job) if job.goal_waiting);
    Ok(Some((
        super::super::StateAdmission { lease, owner },
        native,
    )))
}

pub(super) async fn state(
    worker: &CompletionWorker,
    entry: &Entry,
    mode: Processing<'_>,
) -> Result<(), CompletionWorkerError> {
    if entry.source == Source::AsyncOrphan {
        let Processing::Staged(admission) = mode else {
            return Err(CompletionWorkerError::Held(
                "orphan discovery needs owned state admission".into(),
            ));
        };
        if admission.lease.target() != entry.target {
            return Err(CompletionWorkerError::Held(
                "orphan discovery target lease changed".into(),
            ));
        }
        return admission
            .lease
            .reconcile_orphan_history()
            .await
            .map_err(Into::into);
    }
    if entry.source == Source::Queue {
        return worker.recover_lane(&entry.target, mode).await;
    }
    let Some(Payload::Observed { generation, json }) = load(worker, entry)? else {
        return Ok(());
    };
    let result = async {
        let value = serde_json::from_str(&json)
            .map_err(|e| CompletionWorkerError::Delivery(e.to_string()))?;
        let completion = cdr_app_server::outcomes::parse_turn_completion(&value, false)?;
        worker
            .finish_with_owner_mode(
                worker.server.generation(),
                generation,
                &completion,
                None,
                mode,
            )
            .await
    }
    .await;
    if let Err(error) = &result {
        cdr_store::observed_completion::record_error(
            worker.queue.db_path(),
            &entry.target,
            &entry.turn,
            &error.to_string(),
        )?;
    }
    result
}

pub(super) async fn deliver(
    worker: &CompletionWorker,
    entry: &Entry,
) -> Result<(), CompletionWorkerError> {
    let Some(payload) = load(worker, entry)? else {
        return Ok(());
    };
    match payload {
        Payload::Commentary(p) => worker.deliver_commentary(&p).await,
        Payload::Goal(p) => worker.deliver_goal_progress(&p).await,
        Payload::Final(p) => worker.deliver_one(&p).await,
        Payload::Question(p) => {
            crate::async_question_ui::deliver_checked(
                worker.queue.db_path(),
                worker.server.generation(),
                &worker.http,
                &p,
            )
            .await
        }
        Payload::StartFailure(p) => {
            super::super::receipt::send_chunk(
                worker.queue.db_path(),
                &worker.http,
                i64_channel(p.channel_id)?,
                &IdempotentChunk {
                    domain: cdr_store::reserve_policy::start_notice::DOMAIN,
                    logical_key: p.job_id.clone(),
                    chunk_index: 0,
                    content: p.content,
                },
            )
            .await?;
            cdr_store::reserve_policy::start_notice::complete(worker.queue.db_path(), &p.job_id)?;
            Ok(())
        }
        Payload::Observed { .. } => Ok(()),
    }
}

pub(super) async fn maintain(worker: &CompletionWorker) -> Result<(), CompletionWorkerError> {
    // Global maintenance is single-flight and never includes a Discord POST.
    crate::async_question_ui::prepare_pending(
        worker.queue.db_path(),
        worker.server.instance_id(),
        worker.server.generation(),
    )?;
    let (_permit, _draining) = match worker.queue.enter_background_recovery() {
        Ok(Some(pair)) => (Some(pair.0), pair.1),
        Ok(None) => (None, false),
        Err(crate::queue_runner::QueueRunnerError::RestartDrain(
            crate::restart_readiness::drain::DrainGateError::Sealed,
        )) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if crate::queue_recovery_transport::stabilize_after_queue_recovery(worker.server.as_ref())
        .await?
    {
        eprintln!(
            "completion_transport_stabilized generation={}",
            worker.server.generation()
        );
    }
    Ok(())
}
