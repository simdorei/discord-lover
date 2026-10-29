//! Retain native process ownership until the complete owned tree is confirmed gone.
use super::{
    AgyError, Result,
    runner::process::{Child, Reader},
};
use std::time::Duration;
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Clone, Default)]
pub(super) struct Processes(Arc<Mutex<HashMap<String, Child>>>);
impl Processes {
    pub fn launch(
        &self,
        id: &str,
        exe: &Path,
        args: &[String],
        cwd: &Path,
    ) -> Result<(Reader, Reader)> {
        let mut processes = self
            .0
            .lock()
            .map_err(|_| AgyError::Invalid("AGY process custody unavailable".into()))?;
        if processes.contains_key(id) {
            return Err(AgyError::Invalid("AGY process already exists".into()));
        }
        let (child, out, err) = super::runner::process::launch(exe, args, cwd)?;
        processes.insert(id.into(), child);
        Ok((out, err))
    }
    pub fn count(&self) -> usize {
        self.0.lock().map_or(usize::MAX, |p| p.len())
    }
    pub fn try_wait(&self, id: &str) -> Result<Option<i32>> {
        self.0
            .lock()
            .map_err(|_| AgyError::Invalid("AGY process custody unavailable".into()))?
            .get_mut(id)
            .ok_or_else(|| AgyError::Invalid("AGY process custody missing".into()))?
            .try_wait()
    }
    pub fn kill(&self, id: &str) -> Result<()> {
        if let Some(child) = self
            .0
            .lock()
            .map_err(|_| AgyError::Invalid("AGY process custody unavailable".into()))?
            .get_mut(id)
        {
            child.kill()?;
        }
        Ok(())
    }
    fn reap(&self, id: &str) -> Result<bool> {
        let mut all = self
            .0
            .lock()
            .map_err(|_| AgyError::Invalid("AGY process custody unavailable".into()))?;
        let Some(child) = all.get_mut(id) else {
            return Ok(true);
        };
        if child.terminate_tree_now()? {
            all.remove(id);
            Ok(true)
        } else {
            Ok(false)
        }
    }
    pub async fn cleanup(&self, id: &str) -> Result<()> {
        self.kill(id)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if self.reap(id)? {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AgyError::CleanupUnconfirmed);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    pub async fn cleanup_all(&self) -> Result<()> {
        let ids: Vec<String> = self
            .0
            .lock()
            .map_err(|_| AgyError::Invalid("AGY process custody unavailable".into()))?
            .keys()
            .cloned()
            .collect();
        let mut tasks = tokio::task::JoinSet::new();
        for id in ids {
            let owner = self.clone();
            tasks.spawn(async move { owner.cleanup(&id).await });
        }
        let mut failure = None;
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => failure = Some(e),
                Err(_) => failure = Some(AgyError::CleanupUnconfirmed),
            }
        }
        failure.map_or(Ok(()), Err)
    }
}
