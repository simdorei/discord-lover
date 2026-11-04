//! Bounded, disposable hints. The queue row and existing recovery gates remain authority.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use cdr_store::queue::{StoredQueueJob, list_filtered};

use super::retry::{
    eligible_pending_head, is_definite_writer_rejection, pending_retry_due_at, unix_now,
};
use super::{QueueCoordinator, QueueRunnerError, TargetLease, TurnBackend};

const HINT_CAP: usize = 128;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WriterRetryHint {
    pub(crate) target: String,
    job_id: String,
    generation: i64,
    attempt: i64,
    updated_at: f64,
    due: Instant,
}

#[derive(Default)]
pub(super) struct WriterRetryDeadlines(BTreeMap<String, WriterRetryHint>);

impl<B: TurnBackend> QueueCoordinator<B> {
    pub(super) fn schedule_writer_retry(
        &self,
        job: &StoredQueueJob,
    ) -> Result<(), QueueRunnerError> {
        let target = &job.target_thread_id;
        let memory_gate = self
            .recovery_state
            .lock()
            .map_err(|_| QueueRunnerError::LockPoisoned)?
            .unavailable_logs
            .retry_at(target);
        let mut hints = self
            .writer_retries
            .lock()
            .map_err(|_| QueueRunnerError::LockPoisoned)?;
        if !is_definite_writer_rejection(&job.last_error) {
            hints.0.remove(target);
            return Ok(());
        }
        // A full hint cache is not lost work: the normal paged Queue scan rediscovers it.
        if (hints.0.len() >= HINT_CAP && !hints.0.contains_key(target))
            || target.len() + job.job_id.len() > cdr_store::completion_work::MAX_METADATA_BYTES
        {
            return Ok(());
        }
        let Some(wall_due) =
            pending_retry_due_at(job.attempt_count, &job.last_error, job.updated_at)
        else {
            return Ok(());
        };
        let delay = Duration::try_from_secs_f64((wall_due - unix_now()?).max(0.0))
            .unwrap_or(Duration::from_mins(15));
        let now = Instant::now();
        let due = now
            .checked_add(delay)
            .unwrap_or(now)
            .max(memory_gate.unwrap_or(now));
        hints.0.insert(
            target.clone(),
            WriterRetryHint {
                target: target.clone(),
                job_id: job.job_id.clone(),
                generation: job.app_server_generation,
                attempt: job.attempt_count,
                updated_at: job.updated_at,
                due,
            },
        );
        Ok(())
    }

    pub(super) fn extend_writer_retry_gate(&self, target: &str) -> Result<(), QueueRunnerError> {
        let memory_gate = self
            .recovery_state
            .lock()
            .map_err(|_| QueueRunnerError::LockPoisoned)?
            .unavailable_logs
            .retry_at(target);
        let mut hints = self
            .writer_retries
            .lock()
            .map_err(|_| QueueRunnerError::LockPoisoned)?;
        if let (Some(hint), Some(gate)) = (hints.0.get_mut(target), memory_gate) {
            hint.due = hint.due.max(gate);
        }
        Ok(())
    }

    pub(crate) fn due_writer_retries(&self) -> Result<Vec<WriterRetryHint>, QueueRunnerError> {
        let now = Instant::now();
        let hints = self
            .writer_retries
            .lock()
            .map_err(|_| QueueRunnerError::LockPoisoned)?;
        // Keep a hint until admitted; a busy target or full Ready queue cannot consume it.
        Ok(hints
            .0
            .values()
            .filter(|hint| hint.due <= now)
            .cloned()
            .collect())
    }
}

impl<B: TurnBackend> TargetLease<'_, B> {
    pub(crate) fn consume_writer_retry(
        &self,
        expected: &WriterRetryHint,
    ) -> Result<bool, QueueRunnerError> {
        self.require_target(&expected.target)?;
        {
            let mut hints = self
                .queue
                .writer_retries
                .lock()
                .map_err(|_| QueueRunnerError::LockPoisoned)?;
            if hints.0.get(&expected.target) != Some(expected) {
                return Ok(false);
            }
            hints.0.remove(&expected.target);
        }
        let jobs = cdr_store::execution_hold::eligible_jobs(
            &self.queue.db_path,
            list_filtered(&self.queue.db_path, Some(&expected.target), None)?,
        )?;
        let Some(head) = eligible_pending_head(&jobs) else {
            return Ok(false);
        };
        if head.job_id != expected.job_id
            || head.app_server_generation != expected.generation
            || head.attempt_count != expected.attempt
            || head.updated_at.to_bits() != expected.updated_at.to_bits()
            || !is_definite_writer_rejection(&head.last_error)
        {
            return Ok(false);
        }
        // Recovery rechecks both time gates and all holds, generation and active-turn gates.
        Ok(true)
    }
}

#[cfg(test)]
#[path = "retry_deadlines_tests.rs"]
mod tests;
