use super::{
    Result,
    blocking::BlockingWorker,
    store::{Session, Store},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub(super) struct Database {
    pub worker: BlockingWorker,
    store: Arc<Store>,
}

impl Database {
    pub async fn open(worker: BlockingWorker, path: PathBuf, guild: u64) -> Result<Self> {
        let store = Arc::new(worker.call(move || Store::open(&path, guild)).await?);
        Ok(Self { worker, store })
    }
    pub async fn call<T: Send + 'static>(
        &self,
        op: impl FnOnce(&Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let store = Arc::clone(&self.store);
        self.worker.call(move || op(&store)).await
    }
    pub async fn container(&self, key: &str) -> Result<Option<Option<u64>>> {
        let key = key.to_owned();
        self.call(move |s| s.container(&key)).await
    }
    pub async fn begin_container(&self, key: &str, parent: Option<u64>, kind: &str) -> Result<()> {
        let (key, kind) = (key.to_owned(), kind.to_owned());
        self.call(move |s| s.begin_container(&key, parent, &kind))
            .await
    }
    pub async fn confirm_container(&self, key: &str, id: u64) -> Result<()> {
        let key = key.to_owned();
        self.call(move |s| s.confirm_container(&key, id)).await
    }
    pub async fn bind_project(&self, name: &str, cwd: &Path) -> Result<()> {
        let (name, cwd) = (name.to_owned(), cwd.to_owned());
        self.call(move |s| s.bind_project(&name, &cwd)).await
    }
    pub async fn sessions(&self) -> Result<Vec<Session>> {
        self.call(Store::sessions).await
    }
    pub async fn session(&self, id: u64) -> Result<Option<Session>> {
        self.call(move |s| s.session(id)).await
    }
    pub async fn create_session(&self, id: u64, project: &str, cwd: &Path) -> Result<()> {
        let (project, cwd) = (project.to_owned(), cwd.to_owned());
        self.call(move |s| s.create_session(id, &project, &cwd))
            .await
    }
    pub async fn save_conversation(&self, id: u64, conversation: &str) -> Result<()> {
        let conversation = conversation.to_owned();
        self.call(move |s| s.save_conversation(id, &conversation))
            .await
    }
    pub async fn claim(&self, id: &str, origin: u64) -> Result<bool> {
        let id = id.to_owned();
        self.call(move |s| s.claim(&id, origin)).await
    }
    pub async fn running(&self, id: &str, target: u64) -> Result<()> {
        let id = id.to_owned();
        self.call(move |s| s.running(&id, target)).await
    }
    pub async fn finish(&self, id: &str, state: &str, result: &str) -> Result<()> {
        let (id, state, result) = (id.to_owned(), state.to_owned(), result.to_owned());
        self.call(move |s| s.finish(&id, &state, &result)).await
    }
    pub async fn block_unbound_session(&self, id: u64) -> Result<()> {
        self.call(move |s| s.block_unbound_session(id)).await
    }
    pub async fn last_result(&self, id: u64) -> Result<Option<(String, String)>> {
        self.call(move |s| s.last_result(id)).await
    }
}
