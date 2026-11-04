use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use cdr_store::queue::{QueueJobState, list_filtered};
use tokio::sync::Mutex as AsyncMutex;

use crate::restart_readiness::drain::AdmissionGate;
use unavailable_log::UnavailableTargetLogState;

mod completion;
mod fork_handoff;
mod prompt_intake_submission;
mod recovery;
mod recovery_readiness;
mod retry;
mod retry_deadlines;
pub(crate) use retry_deadlines::WriterRetryHint;
mod submission;
mod target_lease;
mod types;
pub use recovery_readiness::RecoveryReadiness;
pub(crate) use target_lease::TargetLease;
mod unavailable_log;

pub use fork_handoff::AppServerTarget;
pub use recovery::RecoveryReport;
pub use retry::{
    is_definite_writer_rejection, pending_retry_due_at, pending_retry_is_due,
    retry_delay_for_error, retry_delay_seconds,
};
pub use types::{
    BackendFailure, BackendFailureKind, BoxBackendFuture, BusyStatus, QueueRunnerError, Submission,
    TurnBackend, TurnRecord, is_active_writer_message,
};

pub struct QueueCoordinator<B: TurnBackend> {
    pub(super) db_path: PathBuf,
    pub(super) backend: Arc<B>,
    pub(super) target_locks: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    pub(super) recovery_state: Mutex<RecoveryState>,
    pub(super) admission: Option<AdmissionGate>,
    delivery_ready: tokio::sync::Notify,
    writer_retries: Mutex<retry_deadlines::WriterRetryDeadlines>,
}

#[derive(Default)]
pub(super) struct RecoveryState {
    initialized: bool,
    cold_targets: BTreeSet<String>,
    unavailable_logs: UnavailableTargetLogState,
}

impl<B: TurnBackend> QueueCoordinator<B> {
    #[must_use]
    pub fn new(db_path: PathBuf, backend: Arc<B>) -> Self {
        Self {
            db_path,
            backend,
            target_locks: Mutex::new(HashMap::new()),
            recovery_state: Mutex::new(RecoveryState::default()),
            admission: None,
            delivery_ready: tokio::sync::Notify::new(),
            writer_retries: Mutex::default(),
        }
    }

    #[must_use]
    pub fn new_with_admission_gate(
        db_path: PathBuf,
        backend: Arc<B>,
        admission: AdmissionGate,
    ) -> Self {
        let mut coordinator = Self::new(db_path, backend);
        coordinator.admission = Some(admission);
        coordinator
    }

    pub(crate) fn notify_delivery_ready(&self) {
        self.delivery_ready.notify_one();
    }

    pub(crate) async fn wait_for_delivery_ready(&self) {
        self.delivery_ready.notified().await;
    }

    pub async fn busy_status(
        &self,
        target_thread_id: &str,
    ) -> Result<BusyStatus, QueueRunnerError> {
        let lock = self.target_lock(target_thread_id)?;
        let _guard = lock.lock().await;
        let active = self
            .backend
            .active_turn_id(target_thread_id)
            .await
            .map_err(QueueRunnerError::Backend)?
            .is_some();
        let queued = cdr_store::execution_hold::eligible_jobs(
            &self.db_path,
            list_filtered(&self.db_path, Some(target_thread_id), None)?,
        )?
        .iter()
        .any(|job| job.state != QueueJobState::Quarantined);
        Ok(BusyStatus {
            busy: active || queued,
            allow_steer: active,
        })
    }

    pub async fn kick_target(&self, target_thread_id: &str) -> Result<(), QueueRunnerError> {
        let lock = self.target_lock(target_thread_id)?;
        let _guard = lock.lock().await;
        let generation = generation_i64(self.backend.generation())?;
        let _ = self.start_next_locked(target_thread_id, generation).await?;
        Ok(())
    }

    pub async fn control_binding(
        &self,
        target: &str,
    ) -> Result<(Option<String>, Option<String>), QueueRunnerError> {
        let active = self
            .backend
            .active_turn_id(target)
            .await
            .map_err(QueueRunnerError::Backend)?;
        if active.is_some() {
            return Ok((active, None));
        }
        let jobs = list_filtered(&self.db_path, Some(target), None)?;
        let candidates: Vec<_> = jobs
            .iter()
            .filter(|job| {
                matches!(job.state, QueueJobState::Starting | QueueJobState::Running)
                    && !job.goal_waiting
            })
            .collect();
        Ok(if candidates.len() == 1 {
            (
                candidates[0].turn_id.clone(),
                Some(candidates[0].job_id.clone()),
            )
        } else {
            (None, None)
        })
    }

    pub(crate) fn target_lock(
        &self,
        target: &str,
    ) -> Result<Arc<AsyncMutex<()>>, QueueRunnerError> {
        let mut locks = self
            .target_locks
            .lock()
            .map_err(|_| QueueRunnerError::LockPoisoned)?;
        // Owners and queued waiters hold strong references. The registry must not
        // keep one allocation and target string for every historical thread.
        locks.retain(|_, lock| lock.strong_count() != 0);
        if let Some(existing) = locks.get(target).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(target.to_owned(), Arc::downgrade(&lock));
        Ok(lock)
    }
}

pub(super) fn generation_i64(generation: u64) -> Result<i64, QueueRunnerError> {
    i64::try_from(generation).map_err(|_| QueueRunnerError::IntegerRange)
}

fn id_i64(id: u64) -> Result<i64, QueueRunnerError> {
    i64::try_from(id).map_err(|_| QueueRunnerError::IntegerRange)
}

#[cfg(test)]
#[path = "queue_runner/lock_cache_tests.rs"]
mod lock_cache_tests;
