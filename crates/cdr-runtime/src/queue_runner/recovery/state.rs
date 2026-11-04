use std::collections::BTreeSet;
use std::time::Instant;

use cdr_store::queue::{QueueJobState, StoredQueueJob};

use super::RecoveryReport;
use crate::queue_runner::{
    BackendFailure, BackendFailureKind, QueueCoordinator, QueueRunnerError, TurnBackend,
};

impl<B: TurnBackend> QueueCoordinator<B> {
    pub(super) fn initialize_recovery(
        &self,
        targets: &BTreeSet<String>,
    ) -> Result<(), QueueRunnerError> {
        let mut state = self
            .recovery_state
            .lock()
            .map_err(|_| QueueRunnerError::LockPoisoned)?;
        state.unavailable_logs.retain_targets(targets);
        if !state.initialized {
            state.initialized = true;
            state.cold_targets.extend(targets.iter().cloned());
        }
        Ok(())
    }

    pub(super) fn unavailable_retry_due(
        &self,
        target: &str,
        now: Instant,
    ) -> Result<bool, QueueRunnerError> {
        Ok(self
            .recovery_state
            .lock()
            .map_err(|_| QueueRunnerError::LockPoisoned)?
            .unavailable_logs
            .retry_due(target, now))
    }

    pub(super) fn is_cold_target(&self, target: &str) -> Result<bool, QueueRunnerError> {
        let state = self
            .recovery_state
            .lock()
            .map_err(|_| QueueRunnerError::LockPoisoned)?;
        Ok(state.cold_targets.contains(target))
    }

    pub(super) fn mark_reconciled(&self, target: &str) -> Result<(), QueueRunnerError> {
        let mut state = self
            .recovery_state
            .lock()
            .map_err(|_| QueueRunnerError::LockPoisoned)?;
        state.cold_targets.remove(target);
        Ok(())
    }

    pub(super) fn mark_read_unavailable(
        &self,
        jobs: &[StoredQueueJob],
        target: &str,
        error: &BackendFailure,
        report: &mut RecoveryReport,
    ) -> Result<(), QueueRunnerError> {
        report.read_unavailable_targets.insert(target.into());
        self.mark_unavailable(jobs, target, error, report)
    }

    pub(super) fn mark_mutation_unavailable(
        &self,
        jobs: &[StoredQueueJob],
        target: &str,
        error: &BackendFailure,
        report: &mut RecoveryReport,
    ) -> Result<(), QueueRunnerError> {
        report.mutation_unavailable_targets.insert(target.into());
        if error.kind == BackendFailureKind::ActiveWriter {
            report.active_writer_targets.insert(target.into());
        }
        self.mark_unavailable(jobs, target, error, report)
    }

    fn mark_unavailable(
        &self,
        jobs: &[StoredQueueJob],
        target: &str,
        error: &BackendFailure,
        report: &mut RecoveryReport,
    ) -> Result<(), QueueRunnerError> {
        let affected = jobs
            .iter()
            .filter(|job| job.target_thread_id == target && job.state != QueueJobState::Quarantined)
            .count();
        report.unresolved += affected;
        report.unavailable_targets.insert(target.into());
        let decision = self
            .recovery_state
            .lock()
            .map_err(|_| QueueRunnerError::LockPoisoned)?
            .unavailable_logs
            .on_failure(target, &error.message, Instant::now());
        if let Some(decision) = decision {
            eprintln!(
                "rust_queue_recovery_target_unavailable target={target} jobs={affected} error={} suppressed={}",
                decision.error, decision.suppressed
            );
        }
        self.extend_writer_retry_gate(target)?;
        Ok(())
    }

    pub(super) fn clear_unavailable_log(&self, target: &str) -> Result<(), QueueRunnerError> {
        self.recovery_state
            .lock()
            .map_err(|_| QueueRunnerError::LockPoisoned)?
            .unavailable_logs
            .on_success(target);
        Ok(())
    }
}
