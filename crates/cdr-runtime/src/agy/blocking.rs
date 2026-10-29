//! A single bounded OS worker owns optional AGY filesystem/SQLite work.
//! A timeout abandons the response wait, never pretends to cancel a commit.
use super::{AgyError, Result};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub const DB_BUDGET: Duration = Duration::from_millis(250);
// Leave scheduling/publication headroom inside the 100 ms startup contract.
pub const OWNERSHIP_BUDGET: Duration = Duration::from_millis(75);
const CAPACITY: usize = 64;
type Work = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Progress {
    outstanding: AtomicUsize,
    uncertain: AtomicBool,
    stopped: AtomicBool,
}

#[derive(Clone)]
pub(super) struct BlockingWorker {
    sender: mpsc::Sender<Work>,
    progress: Arc<Progress>,
}

struct Ticket(Arc<Progress>);
struct Awaiting {
    progress: Arc<Progress>,
    answered: bool,
}
impl Drop for Awaiting {
    fn drop(&mut self) {
        if !self.answered {
            self.progress.uncertain.store(true, Ordering::Release);
        }
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        self.0.outstanding.fetch_sub(1, Ordering::AcqRel);
    }
}

impl BlockingWorker {
    pub fn start() -> Result<Self> {
        let (sender, mut receiver) = mpsc::channel::<Work>(CAPACITY);
        let progress = Arc::new(Progress::default());
        let worker_progress = Arc::clone(&progress);
        std::thread::Builder::new()
            .name("agy-storage".into())
            .spawn(move || {
                while let Some(work) = receiver.blocking_recv() {
                    work();
                }
                worker_progress.stopped.store(true, Ordering::Release);
            })?;
        Ok(Self { sender, progress })
    }

    pub fn idle(&self) -> bool {
        self.progress.outstanding.load(Ordering::Acquire) == 0
    }
    pub fn uncertain(&self) -> bool {
        self.progress.uncertain.load(Ordering::Acquire)
    }

    /// The manager must also prove execution/process quiescence before recovery.
    pub fn allow_recovery_after_quiescence(&self) -> Result<()> {
        if !self.idle() || self.progress.stopped.load(Ordering::Acquire) {
            return Err(AgyError::Invalid(
                "AGY storage is still settling; recovery is blocked".into(),
            ));
        }
        self.progress.uncertain.store(false, Ordering::Release);
        Ok(())
    }

    pub async fn call<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.call_with_budget(DB_BUDGET, work).await
    }

    pub async fn call_with_budget<T: Send + 'static>(
        &self,
        budget: Duration,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        if self.uncertain() {
            return Err(AgyError::Invalid(
                "AGY storage outcome is uncertain; cleanup/recovery required".into(),
            ));
        }
        let (sender, receiver) = oneshot::channel();
        self.progress.outstanding.fetch_add(1, Ordering::AcqRel);
        let ticket = Ticket(Arc::clone(&self.progress));
        let progress = Arc::clone(&self.progress);
        self.sender
            .try_send(Box::new(move || {
                let _ticket = ticket;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                    .unwrap_or_else(|_| {
                        progress.uncertain.store(true, Ordering::Release);
                        Err(AgyError::Invalid("AGY storage task panicked".into()))
                    });
                // A lost receiver never rolls back or repeats this operation.
                let _ = sender.send(result);
            }))
            .map_err(|_| AgyError::Invalid("AGY storage queue is full or closed".into()))?;
        let mut awaiting = Awaiting {
            progress: Arc::clone(&self.progress),
            answered: false,
        };
        if let Ok(Ok(result)) = tokio::time::timeout(budget, receiver).await {
            awaiting.answered = true;
            result
        } else {
            self.progress.uncertain.store(true, Ordering::Release);
            Err(AgyError::Invalid(
                "AGY storage response timed out; outcome is uncertain, not rolled back".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn late_commit_is_not_replayed_or_replaced_and_does_not_block_async_work() {
        let worker = BlockingWorker::start().unwrap();
        let (release, held) = std::sync::mpsc::channel();
        let committed = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&committed);
        let result = worker
            .call_with_budget(Duration::from_millis(10), move || {
                held.recv().unwrap();
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(result.is_err());
        assert!(!worker.idle());
        assert!(worker.allow_recovery_after_quiescence().is_err());
        assert!(worker.call(|| Ok(())).await.is_err());
        let codex = tokio::spawn(async { 42 });
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), codex)
                .await
                .unwrap()
                .unwrap(),
            42
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !worker.idle() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(committed.load(Ordering::SeqCst), 1);
        worker.allow_recovery_after_quiescence().unwrap();
        assert_eq!(worker.call(|| Ok(7)).await.unwrap(), 7);
    }
}
