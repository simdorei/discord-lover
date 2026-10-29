//! Local commands use OS pipe identity, a bounded replay ledger and runtime-owned writes.
use super::{
    AgyError, Result,
    lifecycle::Status,
    manager::{AgyManager, Command},
};
use cdr_windows_native::local_control as native;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, NamedPipeServer},
    sync::{Mutex, mpsc, watch},
    task::{JoinHandle, JoinSet},
};
mod ledger;
use ledger::{Decision, Ledger};
const PROTOCOL: u32 = 1;
const MAX_FRAME: usize = 128 * 1024;
const MARKER: &str = ".codex_discord_agy.control.json";
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Binding {
    protocol: u32,
    root: PathBuf,
    runtime_uuid: String,
    pid: u32,
    creation: u64,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Status,
    Settings,
    Mutate { id: String, command: Command },
    Result { id: String },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    binding: Binding,
    operation: Operation,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Response {
    binding: Binding,
    pub status: Status,
    pub outcome: String,
    pub error: Option<String>,
    pub settings: Option<super::config::Settings>,
}
pub(crate) struct Control {
    task: JoinHandle<Result<()>>,
}
impl Control {
    pub(crate) fn start(
        manager: Arc<AgyManager>,
        root: PathBuf,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        let task = tokio::spawn(async move {
            let result = serve(Arc::clone(&manager), root, shutdown).await;
            if let Err(error) = &result {
                manager.fault(format!("AGY 로컬 제어 연결 실패: {error}"));
            }
            result
        });
        Self { task }
    }
    pub(crate) async fn join(mut self, deadline: tokio::time::Instant) -> Result<()> {
        tokio::time::timeout_at(deadline, &mut self.task)
            .await
            .map_err(|_| AgyError::CleanupUnconfirmed)?
            .map_err(|_| AgyError::CleanupUnconfirmed)?
    }
}
fn pipe_name(root: &Path) -> String {
    format!(
        r"\\.\pipe\cdr-agy-{}",
        hex::encode(Sha256::digest(
            root.to_string_lossy().to_lowercase().as_bytes()
        ))
    )
}
struct Mutation {
    id: String,
    command: Command,
}
#[derive(Clone)]
struct Endpoint {
    binding: Binding,
    own: native::Identity,
    manager: Arc<AgyManager>,
    ledger: Arc<Mutex<Ledger>>,
    mutations: mpsc::Sender<Mutation>,
    #[cfg(test)]
    settings_read: Option<Arc<tests::SettingsReadBarrier>>,
}
async fn serve(
    manager: Arc<AgyManager>,
    root: PathBuf,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let root = root.canonicalize()?;
    let own = native::identity(std::process::id())?;
    let binding = Binding {
        protocol: PROTOCOL,
        root: root.clone(),
        runtime_uuid: manager.status().runtime_uuid,
        pid: own.pid,
        creation: own.creation,
    };
    let name = pipe_name(&root);
    let mut listener = native::pipe(&name, true)?;
    let marker = root.join(MARKER);
    let bytes = serde_json::to_vec(&binding)?;
    manager
        .ownership
        .worker()?
        .call(move || {
            use std::io::Write;
            let mut file =
                tempfile::NamedTempFile::new_in(marker.parent().expect("marker parent"))?;
            file.write_all(&bytes)?;
            file.as_file().sync_all()?;
            cdr_windows_native::atomic_replace(file.path(), &marker)?;
            Ok(())
        })
        .await?;
    let (mutations, mut requests) = mpsc::channel(32);
    let endpoint = Endpoint {
        binding,
        own,
        manager,
        ledger: Arc::new(Mutex::new(Ledger::default())),
        mutations,
        #[cfg(test)]
        settings_read: None,
    };
    let mut clients = JoinSet::new();
    let mut changes = JoinSet::new();
    let mut listener_failure = None;
    loop {
        tokio::select! {
            biased;
            _=shutdown.changed()=>break,
            Some(_)=clients.join_next(),if !clients.is_empty()=>{},
            Some(result)=changes.join_next(),if !changes.is_empty()=>{if result.is_err(){endpoint.manager.fault("AGY 제어 작업이 비정상 종료되었습니다.".into());}},
            Some(request)=requests.recv(),if changes.len()<32=>{
                let endpoint=endpoint.clone();changes.spawn(async move {
                    use futures_util::FutureExt;
                    let result=std::panic::AssertUnwindSafe(endpoint.manager.command(request.command)).catch_unwind().await
                        .unwrap_or_else(|_|Err("AGY command task panicked; status must be rechecked".into()));
                    endpoint.ledger.lock().await.complete(&request.id,result.map(|_|()),native::monotonic_millis());
                });
            }
            result=listener.connect(),if clients.len()<12=>{
                if let Err(error)=result { listener_failure=Some(error);break; }
                let next=match native::pipe(&name,false) {
                    Ok(pipe)=>pipe,
                    Err(error)=>{listener_failure=Some(error);break;}
                };
                let connected=std::mem::replace(&mut listener,next);
                let endpoint=endpoint.clone();clients.spawn(async move {
                    // Only connection I/O is cancelled. Accepted mutations belong to changes.
                    let _=tokio::time::timeout(Duration::from_secs(3),endpoint.connection(connected)).await;
                });
            }
        }
        if *shutdown.borrow() {
            break;
        }
    }
    drop(listener);
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    requests.close();
    while let Ok(request) = requests.try_recv() {
        endpoint.ledger.lock().await.complete(
            &request.id,
            Err("브릿지 종료 중: 접수되지 않았습니다.".into()),
            native::monotonic_millis(),
        );
    }
    while changes.join_next().await.is_some() {}
    // Stale marker is harmless: clients also check PID, creation identity and live binding.
    listener_failure.map_or(Ok(()), |error| Err(error.into()))
}
impl Endpoint {
    async fn connection(&self, mut pipe: NamedPipeServer) -> Result<()> {
        let peer = native::peer(&pipe, true)?;
        if peer.sid != self.own.sid || peer.executable != self.own.executable {
            return Err(AgyError::Invalid("Untrusted AGY control client".into()));
        }
        let request: Request = read_frame(&mut pipe).await?;
        if native::peer(&pipe, true)? != peer || request.binding != self.binding {
            return Err(AgyError::Invalid(
                "AGY control runtime/peer identity changed".into(),
            ));
        }
        let mut response = Response {
            binding: self.binding.clone(),
            status: self.manager.status(),
            outcome: "status".into(),
            error: None,
            settings: None,
        };
        match request.operation {
            Operation::Status => {}
            Operation::Settings => {
                response.settings = self.manager.settings()?;
                #[cfg(test)]
                if let Some(barrier) = &self.settings_read {
                    barrier.read.notify_one();
                    barrier.release.notified().await;
                }
            }
            Operation::Result { id } => {
                let (outcome, error) = self
                    .ledger
                    .lock()
                    .await
                    .lookup(&id, native::monotonic_millis());
                response.outcome = outcome;
                response.error = error;
            }
            Operation::Mutate { id, command } => {
                let payload = serde_json::to_vec(&command)?;
                let decision = self.ledger.lock().await.reserve(
                    &self.binding.runtime_uuid,
                    &id,
                    &payload,
                    native::monotonic_millis(),
                );
                match decision {
                    Decision::New => {
                        if self
                            .mutations
                            .try_send(Mutation {
                                id: id.clone(),
                                command,
                            })
                            .is_err()
                        {
                            self.ledger.lock().await.complete(
                                &id,
                                Err("AGY 제어 대기열이 가득 찼습니다.".into()),
                                native::monotonic_millis(),
                            );
                        }
                    }
                    Decision::Existing => {}
                    Decision::Rejected(error) => {
                        response.outcome = "rejected".into();
                        response.error = Some(error);
                        return write_frame(&mut pipe, &response).await;
                    }
                }
                let (outcome, error) = self
                    .ledger
                    .lock()
                    .await
                    .lookup(&id, native::monotonic_millis());
                response.outcome = outcome;
                response.error = error;
            }
        }
        response.status = self.manager.status();
        write_frame(&mut pipe, &response).await
    }
}
async fn read_frame<T: serde::de::DeserializeOwned>(
    io: &mut (impl AsyncRead + Unpin),
) -> Result<T> {
    let size = io.read_u32_le().await? as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(AgyError::Invalid(
            "AGY control frame too large/empty".into(),
        ));
    }
    let mut bytes = vec![0; size];
    io.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}
async fn write_frame(io: &mut (impl AsyncWrite + Unpin), value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME {
        return Err(AgyError::Invalid("AGY control response too large".into()));
    }
    io.write_u32_le(u32::try_from(bytes.len()).expect("bounded control frame"))
        .await?;
    io.write_all(&bytes).await?;
    io.flush().await?;
    Ok(())
}
pub async fn client(root: &Path, operation: Operation) -> Result<Response> {
    let root = root.canonicalize()?;
    let binding: Binding =
        serde_json::from_slice(&super::config::read_bounded(&root.join(MARKER))?)?;
    if binding.protocol != PROTOCOL || binding.root != root {
        return Err(AgyError::Invalid(
            "AGY control root/protocol mismatch".into(),
        ));
    }
    let own = native::identity(std::process::id())?;
    let connect_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut pipe = loop {
        match ClientOptions::new().open(pipe_name(&root)) {
            Ok(pipe) => break pipe,
            Err(error)
                if error.raw_os_error() == Some(231)
                    && tokio::time::Instant::now() < connect_deadline =>
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let peer = native::peer(&pipe, false)?;
    if peer.pid != binding.pid
        || peer.creation != binding.creation
        || peer.sid != own.sid
        || peer.executable != own.executable
    {
        return Err(AgyError::Invalid(
            "AGY control server OS identity mismatch".into(),
        ));
    }
    let request = Request {
        binding: binding.clone(),
        operation,
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        write_frame(&mut pipe, &request).await?;
        let response: Response = read_frame(&mut pipe).await?;
        if response.binding != binding
            || response.status.runtime_uuid != binding.runtime_uuid
            || native::peer(&pipe, false)? != peer
        {
            return Err(AgyError::Invalid(
                "AGY control response identity mismatch".into(),
            ));
        }
        Ok(response)
    })
    .await
    .map_err(|_| {
        AgyError::Invalid("AGY control timeout; inspect status/result before retrying".into())
    })?
}
/// Used only by the tray's hidden helper process, never the bot startup path.
pub async fn cli() -> Result<()> {
    use std::io::Read;
    let mut input = Vec::new();
    std::io::stdin()
        .take((MAX_FRAME + 1) as u64)
        .read_to_end(&mut input)?;
    if input.len() > MAX_FRAME {
        return Err(AgyError::Invalid("AGY control input too large".into()));
    }
    let operation: Operation = serde_json::from_slice(&input)?;
    let response = client(&std::env::current_dir()?, operation).await?;
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

#[cfg(test)]
mod tests;
