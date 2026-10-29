use std::{collections::VecDeque, future::Future, time::Duration};

use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, timeout_at},
};

use super::DiscordRuntimeError;
use super::shutdown_deadline::terminate_on_shutdown_timeout;

const ABORT_JOIN_RESERVE: Duration = Duration::from_secs(1);
const EXIT_CHANNEL_CAPACITY: usize = 1;

#[derive(Clone)]
pub(super) struct WorkerExitNotifier {
    sender: mpsc::Sender<&'static str>,
}

pub(super) fn exit_channel() -> (WorkerExitNotifier, mpsc::Receiver<&'static str>) {
    let (sender, receiver) = mpsc::channel(EXIT_CHANNEL_CAPACITY);
    (WorkerExitNotifier { sender }, receiver)
}

struct WorkerExitGuard {
    worker: &'static str,
    notifier: WorkerExitNotifier,
}

impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        let _ = self.notifier.sender.try_send(self.worker);
    }
}

pub(super) struct MonitoredWorker {
    name: &'static str,
    handle: Option<JoinHandle<Result<(), DiscordRuntimeError>>>,
}

impl MonitoredWorker {
    pub(super) fn abort(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }

    pub(super) async fn join(
        &mut self,
    ) -> Result<Result<(), DiscordRuntimeError>, tokio::task::JoinError> {
        let result = self.handle.as_mut().expect("worker handle exists").await;
        self.handle.take();
        result
    }

    pub(super) async fn shutdown_until(self, deadline: Instant) -> Result<(), DiscordRuntimeError> {
        WorkerSet::from_workers([self])
            .shutdown_until(deadline)
            .await
    }
}

impl Drop for MonitoredWorker {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

pub(super) fn spawn_monitored<F>(
    name: &'static str,
    notifier: WorkerExitNotifier,
    future: F,
) -> MonitoredWorker
where
    F: Future<Output = Result<(), DiscordRuntimeError>> + Send + 'static,
{
    let handle = tokio::spawn(async move {
        let _exit = WorkerExitGuard {
            worker: name,
            notifier,
        };
        future.await
    });
    MonitoredWorker {
        name,
        handle: Some(handle),
    }
}

pub(super) struct WorkerSet {
    workers: VecDeque<MonitoredWorker>,
}

pub(super) struct WorkerShutdownReport {
    trigger: Option<Result<(), DiscordRuntimeError>>,
    cleanup: Result<(), DiscordRuntimeError>,
}

impl WorkerShutdownReport {
    pub(super) fn with_optional_cleanup(mut self, result: Result<(), DiscordRuntimeError>) -> Self {
        self.cleanup = self.cleanup.and(result);
        self
    }
    #[cfg(test)]
    pub(super) fn from_parts(
        trigger: Option<Result<(), DiscordRuntimeError>>,
        cleanup: Result<(), DiscordRuntimeError>,
    ) -> Self {
        Self { trigger, cleanup }
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        Option<Result<(), DiscordRuntimeError>>,
        Result<(), DiscordRuntimeError>,
    ) {
        (self.trigger, self.cleanup)
    }
}

impl WorkerSet {
    pub(super) fn from_workers(workers: impl IntoIterator<Item = MonitoredWorker>) -> Self {
        Self {
            workers: workers.into_iter().collect(),
        }
    }

    pub(super) fn push(&mut self, worker: MonitoredWorker) {
        self.workers.push_back(worker);
    }

    pub(super) async fn shutdown_until(self, deadline: Instant) -> Result<(), DiscordRuntimeError> {
        self.shutdown_inner(deadline, None).await.cleanup
    }

    pub(super) async fn shutdown_for_cause(
        self,
        deadline: Instant,
        trigger: Option<&'static str>,
    ) -> WorkerShutdownReport {
        self.shutdown_inner(deadline, trigger).await
    }

    async fn shutdown_inner(
        mut self,
        deadline: Instant,
        trigger: Option<&'static str>,
    ) -> WorkerShutdownReport {
        let trigger = if let Some(trigger) = trigger {
            match self.take(trigger) {
                Some(mut worker) => {
                    let result = normalize_join(trigger, worker.join().await, false);
                    if let Err(error) = &result {
                        eprintln!("primary_runtime_shutdown_error worker={trigger} error={error}");
                    }
                    Some(result)
                }
                None => None,
            }
        } else {
            None
        };
        let cleanup = self.shutdown_remaining(deadline).await;
        WorkerShutdownReport { trigger, cleanup }
    }

    fn take(&mut self, name: &'static str) -> Option<MonitoredWorker> {
        let position = self.workers.iter().position(|worker| worker.name == name)?;
        self.workers.remove(position)
    }

    async fn shutdown_remaining(mut self, deadline: Instant) -> Result<(), DiscordRuntimeError> {
        let drain_deadline = deadline
            .checked_sub(ABORT_JOIN_RESERVE)
            .unwrap_or(deadline)
            .max(Instant::now());
        let mut first_error = None;
        while let Some(mut worker) = self.workers.pop_front() {
            let name = worker.name;
            if let Ok(result) = timeout_at(drain_deadline, worker.join()).await {
                record_join(name, result, false, &mut first_error);
            } else {
                self.workers.push_front(worker);
                break;
            }
        }
        if self.workers.is_empty() {
            return first_error.map_or(Ok(()), Err);
        }

        let timed_out = self
            .workers
            .iter()
            .map(|worker| worker.name)
            .collect::<Vec<_>>();
        for worker in &self.workers {
            worker.abort();
        }
        while let Some(mut worker) = self.workers.pop_front() {
            let name = worker.name;
            if let Ok(result) = timeout_at(deadline, worker.join()).await {
                record_join(name, result, true, &mut first_error);
            } else {
                eprintln!("fatal_runtime_worker_abort_join_timeout worker={name}");
                terminate_on_shutdown_timeout("runtime-worker");
            }
        }
        let workers = timed_out.join(",");
        if let Some(error) = first_error {
            eprintln!("runtime_worker_shutdown_timeout workers={workers}");
            Err(error)
        } else {
            Err(DiscordRuntimeError::WorkerShutdownTimeout { workers })
        }
    }
}

fn record_join(
    worker: &'static str,
    result: Result<Result<(), DiscordRuntimeError>, tokio::task::JoinError>,
    forced: bool,
    first_error: &mut Option<DiscordRuntimeError>,
) {
    if let Err(error) = normalize_join(worker, result, forced) {
        if first_error.is_none() {
            *first_error = Some(error);
        } else {
            eprintln!("secondary_runtime_worker_error worker={worker} error={error}");
        }
    }
}

fn normalize_join(
    worker: &'static str,
    result: Result<Result<(), DiscordRuntimeError>, tokio::task::JoinError>,
    forced: bool,
) -> Result<(), DiscordRuntimeError> {
    let error = match result {
        Ok(Ok(())) => return Ok(()),
        Ok(Err(error)) => error,
        Err(source) if forced && source.is_cancelled() => return Ok(()),
        Err(source) => DiscordRuntimeError::WorkerTask { worker, source },
    };
    Err(error)
}

#[cfg(test)]
#[path = "worker_supervision_tests.rs"]
mod tests;
