use std::time::Instant;

use cdr_store::queue::{
    QueueJobState, StoredQueueJob, adopt_target_generation, list_filtered,
    record_preflight_failure, unresolved_app_server_fork_handoff_for_source,
};

use super::RecoveryReport;
use crate::queue_runner::retry::{eligible_pending_head, pending_job_is_due, unix_now};
use crate::queue_runner::{
    BackendFailure, QueueCoordinator, QueueRunnerError, TurnBackend, TurnRecord,
};

impl<B: TurnBackend> QueueCoordinator<B> {
    pub(super) async fn observe_one_locked(
        &self,
        target: &str,
        generation: i64,
        report: &mut RecoveryReport,
        force_cold: bool,
    ) -> Result<(), QueueRunnerError> {
        if cdr_store::async_question::target_dispatch_held(&self.db_path, target)? {
            self.observe_async_history_locked(target, report).await;
            return Ok(());
        }
        if cdr_store::dead_generation::target_is_held(&self.db_path, target)? {
            return Ok(());
        }
        let jobs = list_filtered(&self.db_path, Some(target), None)?;
        if !jobs
            .iter()
            .any(|job| matches!(job.state, QueueJobState::Starting | QueueJobState::Running))
        {
            return Ok(());
        }
        let turns = match self.backend.read_turns(target).await {
            Ok(turns) => turns,
            Err(error) => {
                self.mark_read_unavailable(&jobs, target, &error, report)?;
                return Ok(());
            }
        };
        self.clear_unavailable_log(target)?;
        let _ =
            self.reconcile_observed_jobs(target, generation, &jobs, &turns, report, force_cold)?;
        Ok(())
    }

    pub(super) async fn mutate_one_locked(
        &self,
        target: &str,
        generation: i64,
        report: &mut RecoveryReport,
    ) -> Result<(), QueueRunnerError> {
        if cdr_store::async_question::target_dispatch_held(&self.db_path, target)? {
            return Ok(());
        }
        if cdr_store::dead_generation::target_is_held(&self.db_path, target)? {
            return Ok(());
        }
        let jobs = cdr_store::execution_hold::eligible_jobs(
            &self.db_path,
            list_filtered(&self.db_path, Some(target), None)?,
        )?;
        if jobs.is_empty() {
            return Ok(());
        }
        let Some(pending) = eligible_pending_head(&jobs).cloned() else {
            return Ok(());
        };
        if let Some(handoff) = unresolved_app_server_fork_handoff_for_source(&self.db_path, target)?
        {
            let failure = BackendFailure::definite(format!(
                "app-server fork handoff {} is unresolved; duplicate fork retry is fenced",
                handoff.handoff_id
            ));
            self.mark_mutation_unavailable(&jobs, target, &failure, report)?;
            return Ok(());
        }
        if !pending_job_is_due(&pending, unix_now()?)
            || !self.unavailable_retry_due(target, Instant::now())?
        {
            self.schedule_writer_retry(&pending)?;
            return Ok(());
        }
        if let Err(error) = self.backend.resume_thread(target).await {
            self.record_pending_preflight(&pending, &error.message)?;
            self.mark_mutation_unavailable(&jobs, target, &error, report)?;
            return Ok(());
        }
        let turns = match self.backend.read_turns(target).await {
            Ok(turns) => turns,
            Err(error) => {
                self.record_pending_preflight(&pending, &error.message)?;
                self.mark_read_unavailable(&jobs, target, &error, report)?;
                return Ok(());
            }
        };
        let adoption = adopt_target_generation(&self.db_path, target, generation)?;
        report.adopted += adoption.adopted_count;
        self.mark_reconciled(target)?;
        match self
            .start_next_recovered_locked(target, generation, &turns)
            .await
        {
            Ok(Some(_)) => {
                report.started += 1;
                self.clear_unavailable_log(target)?;
            }
            Ok(None) => self.clear_unavailable_log(target)?,
            Err(QueueRunnerError::Backend(error)) => {
                let current = list_filtered(&self.db_path, Some(target), None)?;
                self.mark_mutation_unavailable(&current, target, &error, report)?;
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn reconcile_observed_jobs(
        &self,
        target: &str,
        generation: i64,
        jobs: &[StoredQueueJob],
        turns: &[TurnRecord],
        report: &mut RecoveryReport,
        force_cold: bool,
    ) -> Result<bool, QueueRunnerError> {
        let cold = force_cold || self.is_cold_target(target)?;
        for job in jobs {
            match job.state {
                QueueJobState::Starting => {
                    if !self.recover_starting(job, generation, cold, turns, report)? {
                        return Ok(false);
                    }
                }
                QueueJobState::Running => Self::recover_running(job, turns, report),
                QueueJobState::Pending | QueueJobState::Quarantined => {}
            }
        }
        let adoption = adopt_target_generation(&self.db_path, target, generation)?;
        report.adopted += adoption.adopted_count;
        self.mark_reconciled(target)?;
        Ok(true)
    }

    fn record_pending_preflight(
        &self,
        job: &StoredQueueJob,
        error: &str,
    ) -> Result<(), QueueRunnerError> {
        if let Some(recorded) =
            record_preflight_failure(&self.db_path, &job.job_id, job.app_server_generation, error)?
        {
            self.schedule_writer_retry(&recorded)?;
        }
        Ok(())
    }
}
