use super::{
    AgyError, AgyRuntime, PreparedAgy, Result,
    config::{AgyConfig, Settings},
    configuration::Configuration,
    lifecycle::{Phase, Status},
    ownership::AgyOwnership,
};
use crate::{config::RuntimeConfig, restart_readiness::drain::AdmissionGate};
use cdr_discord::gateway::topology::GatewayTopology;
use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use twilight_http::Client;

mod activation;
mod login;
use activation::{Activation, Probe, UnverifiedProbe};

#[derive(Clone)]
pub(crate) struct Resources {
    pub root: PathBuf,
    pub mirror_db: PathBuf,
    pub settings_path: PathBuf,
    pub policy: Arc<RuntimeConfig>,
    pub http: Arc<Client>,
    pub admission: AdmissionGate,
    pub runtime_uuid: String,
    pub topology: GatewayTopology,
    #[cfg(test)]
    pub remote: Option<Arc<dyn crate::mirror_sync::MirrorTransport>>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    On {
        expected_revision: u64,
    },
    Recheck {
        expected_revision: u64,
    },
    Off {
        expected_revision: u64,
        immediate: bool,
    },
    Configure {
        expected_revision: u64,
        config: AgyConfig,
    },
    Login {
        expected_revision: u64,
    },
}
struct Request {
    command: Command,
    reply: oneshot::Sender<std::result::Result<Status, String>>,
}

pub struct AgyManager {
    pub ownership: Arc<AgyOwnership>,
    status: watch::Receiver<Status>,
    service: Arc<RwLock<Option<Arc<AgyRuntime>>>>,
    commands: mpsc::Sender<Request>,
    failures: mpsc::Sender<String>,
    settings: Arc<RwLock<Option<Settings>>>,
}
pub(crate) struct ManagedAgy {
    pub handle: Arc<AgyManager>,
    task: JoinHandle<Result<()>>,
}
pub(super) struct ServiceLease(Arc<AgyRuntime>);
impl std::ops::Deref for ServiceLease {
    type Target = Arc<AgyRuntime>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Drop for ServiceLease {
    fn drop(&mut self) {
        self.0
            .handlers
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

impl AgyManager {
    pub(crate) async fn start(resources: Resources, shutdown: watch::Receiver<bool>) -> ManagedAgy {
        Self::start_with_probe(resources, shutdown, Arc::new(UnverifiedProbe)).await
    }
    async fn start_with_probe(
        resources: Resources,
        shutdown: watch::Receiver<bool>,
        probe: Arc<dyn Probe>,
    ) -> ManagedAgy {
        let ownership =
            AgyOwnership::for_root(&resources.root, resources.policy.guild_id.unwrap_or(0));
        let mut initial = Status::new(resources.runtime_uuid.clone());
        if let Err(error) = ownership.ensure_loaded(&resources.mirror_db).await {
            initial.detail = format!("AGY 소유권 확인 보류: {error}");
        }
        if let Err(error) = ownership.attach_topology(resources.topology.clone()) {
            initial.detail = error.to_string();
        }
        let (status, status_rx) = watch::channel(initial.clone());
        let (commands, receiver) = mpsc::channel(32);
        let (failures, failure_rx) = mpsc::channel(8);
        let handle = Arc::new(Self {
            ownership,
            status: status_rx,
            service: Arc::new(RwLock::new(None)),
            commands,
            failures,
            settings: Arc::new(RwLock::new(None)),
        });
        let mut manager = Manager {
            resources,
            handle: Arc::clone(&handle),
            status,
            view: initial,
            commands: receiver,
            failures: failure_rx,
            writer: None,
            activation: None,
            worker: None,
            probe,
            login: login::Login::default(),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            provisioning: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cleaning: None,
            cleanup_started: None,
        };
        let task = tokio::spawn(async move {
            use futures_util::FutureExt;
            let mut lifecycle_shutdown = shutdown.clone();
            if let Ok(result) = std::panic::AssertUnwindSafe(manager.run(shutdown))
                .catch_unwind()
                .await
            {
                result
            } else {
                manager.view.transition(
                    Phase::FaultedCleaning,
                    "AGY manager panicked; owned work is being cleaned up.",
                );
                manager.publish();
                let cleanup = manager.shutdown_cleanup().await;
                manager.view.transition(
                    Phase::FaultedBlocked,
                    if cleanup.is_ok() {
                        "AGY manager stopped after cleanup. Restart is required."
                    } else {
                        "AGY manager cleanup unconfirmed. Reactivation is blocked."
                    },
                );
                manager.publish();
                // Keep the configuration writer and all process receipts owned while live.
                while !*lifecycle_shutdown.borrow() {
                    if lifecycle_shutdown.changed().await.is_err() {
                        break;
                    }
                }
                cleanup.and(Err(AgyError::Invalid("AGY manager panicked".into())))
            }
        });
        ManagedAgy { handle, task }
    }
    #[must_use]
    pub fn status(&self) -> Status {
        let mut status = self.status.borrow().clone();
        if self.status.has_changed().is_err() {
            status.phase = Phase::FaultedBlocked;
            status.detail = "AGY 관리 작업 종료. 정리 상태를 확인할 수 없습니다.".into();
        }
        status
    }
    pub fn settings(&self) -> Result<Option<Settings>> {
        self.settings
            .read()
            .map(|s| s.clone())
            .map_err(|_| AgyError::Invalid("AGY settings cache unavailable".into()))
    }
    pub(super) fn service(&self) -> Option<Arc<AgyRuntime>> {
        self.service.read().ok().and_then(|s| s.clone())
    }
    pub(super) fn lease(&self) -> Option<ServiceLease> {
        let slot = self.service.read().ok()?;
        if !matches!(self.status().phase, Phase::Ready | Phase::Draining) {
            return None;
        }
        let service = Arc::clone(slot.as_ref()?);
        service
            .handlers
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Some(ServiceLease(service))
    }
    pub(super) fn fault(&self, reason: String) {
        let _ = self.failures.try_send(reason);
    }
    pub async fn command(&self, command: Command) -> std::result::Result<Status, String> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .try_send(Request { command, reply })
            .map_err(|_| "AGY 제어 대기열이 가득 찼거나 종료 중입니다.".to_owned())?;
        receiver.await.map_err(|_| {
            "AGY 제어 결과를 확인할 수 없습니다. 상태를 다시 확인해 주세요.".to_owned()
        })?
    }
}
impl ManagedAgy {
    pub async fn join(mut self, deadline: tokio::time::Instant) -> Result<()> {
        match tokio::time::timeout_at(deadline, &mut self.task).await {
            Ok(result) => result.map_err(|_| AgyError::CleanupUnconfirmed)?,
            // Do not abort or claim success: the task retains process/receipt custody.
            Err(_) => Err(AgyError::CleanupUnconfirmed),
        }
    }
}
struct Worker {
    stop: watch::Sender<bool>,
    task: JoinHandle<Result<()>>,
}
struct Manager {
    resources: Resources,
    handle: Arc<AgyManager>,
    status: watch::Sender<Status>,
    view: Status,
    commands: mpsc::Receiver<Request>,
    failures: mpsc::Receiver<String>,
    writer: Option<Arc<Configuration>>,
    activation: Option<JoinHandle<(u64, Result<Activation>)>>,
    worker: Option<Worker>,
    probe: Arc<dyn Probe>,
    login: login::Login,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    provisioning: Arc<std::sync::atomic::AtomicBool>,
    cleaning: Option<JoinHandle<Result<()>>>,
    cleanup_started: Option<tokio::time::Instant>,
}
impl Manager {
    fn publish(&mut self) {
        self.view.active = self
            .handle
            .service()
            .and_then(|s| s.active().ok().map(|a| a.len()))
            .unwrap_or(0);
        self.view.sequence = self.view.sequence.saturating_add(1);
        self.status.send_replace(self.view.clone());
    }
    fn quiescent(&self) -> bool {
        self.activation.is_none()
            && self.worker.is_none()
            && self.cleaning.is_none()
            && !self.login.active()
            && self.handle.service().is_none_or(|s| s.quiescent())
            && self.handle.ownership.worker().is_ok_and(|w| w.idle())
    }
    fn cache(&mut self, settings: Settings) {
        self.view.config_revision = settings.revision;
        self.view.enabled = settings.enabled;
        if let Ok(mut slot) = self.handle.settings.write() {
            *slot = Some(settings);
        }
    }
    async fn initialize(&mut self) -> Result<()> {
        let path = self.resources.settings_path.clone();
        let writer = self
            .handle
            .ownership
            .worker()?
            .call(move || Configuration::acquire(&path))
            .await?;
        self.writer = Some(Arc::clone(&writer));
        let settings = writer.snapshot()?;
        if let Some(settings) = settings {
            let enabled = settings.enabled;
            self.cache(settings);
            if enabled {
                self.begin_activation()?;
            }
        }
        Ok(())
    }
    async fn run(&mut self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        if let Err(error) = self.initialize().await {
            self.view
                .transition(Phase::SetupRequired, error.to_string());
        }
        self.publish();
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _=shutdown.changed()=>break,
                request=self.commands.recv()=>{
                    let Some(request)=request else {break;};
                    let result=self.apply(request.command).await;
                    self.publish();
                    let _=request.reply.send(result.map(|()|self.view.clone()).map_err(|e|e.to_string()));
                }
                Some(failure)=self.failures.recv()=>{
                    if let Some(service)=self.handle.service() {service.close_admission().await;let _=service.stop_all();}
                    self.view.transition(Phase::FaultedCleaning,failure);
                    self.cancel.store(true,std::sync::atomic::Ordering::Release);self.cleanup_started=Some(tokio::time::Instant::now());
                    if let Some(worker)=&self.worker {let _=worker.stop.send(true);}
                    self.publish();
                }
                _=tick.tick()=>self.advance().await,
            }
            if *shutdown.borrow() {
                break;
            }
        }
        self.shutdown_cleanup().await
    }
    async fn shutdown_cleanup(&mut self) -> Result<()> {
        let mut confirmed = true;
        self.view.enabled = false;
        self.cancel
            .store(true, std::sync::atomic::Ordering::Release);
        self.view.generation = self.view.generation.saturating_add(1);
        if let Some(service) = self.handle.service() {
            service.close_admission().await;
            confirmed &= service.stop_all().is_ok();
        }
        if let Some(worker) = &self.worker {
            let _ = worker.stop.send(true);
        }
        if let Some(task) = self.activation.take() {
            match task.await {
                Ok((_, Ok(Activation::Prepared(prepared, _)))) => {
                    let (_stop, shutdown) = watch::channel(true);
                    confirmed &= prepared.run(shutdown).await.is_ok();
                }
                Ok((_, Ok(Activation::Blocked(_, _)))) => {}
                _ => confirmed = false,
            }
        }
        if let Some(worker) = self.worker.take() {
            confirmed &= matches!(worker.task.await, Ok(Ok(())));
        }
        if let Some(cleaning) = self.cleaning.take() {
            confirmed &= matches!(cleaning.await, Ok(Ok(())));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !matches!(self.login.poll(true), Ok(true)) {
            if tokio::time::Instant::now() >= deadline {
                confirmed = false;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if let Some(service) = self.handle.service() {
            confirmed &= service.processes.cleanup_all().await.is_ok();
        }
        let worker = self.handle.ownership.worker()?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !worker.idle() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if !confirmed || !worker.idle() {
            return Err(AgyError::CleanupUnconfirmed);
        }
        Ok(())
    }
    fn begin_activation(&mut self) -> Result<()> {
        if !self.view.start(self.quiescent())? {
            return Ok(());
        }
        let worker = self.handle.ownership.worker()?;
        worker.allow_recovery_after_quiescence()?;
        let settings = self
            .handle
            .settings()?
            .ok_or_else(|| AgyError::Invalid("AGY 설정이 없습니다.".into()))?;
        let resources = self.resources.clone();
        let probe = Arc::clone(&self.probe);
        let generation = self.view.generation;
        self.cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.provisioning = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel = Arc::clone(&self.cancel);
        let provisioning = Arc::clone(&self.provisioning);
        self.activation = Some(tokio::spawn(async move {
            (
                generation,
                activation::prepare(resources, settings, worker, probe, cancel, provisioning).await,
            )
        }));
        Ok(())
    }
    #[allow(
        clippy::too_many_lines,
        reason = "one cohesive lifecycle or contract scenario preserves ordering visibly"
    )]
    async fn apply(&mut self, command: Command) -> Result<()> {
        if let Command::Off {
            expected_revision, ..
        }
        | Command::Login { expected_revision } = &command
            && *expected_revision != self.view.config_revision
        {
            return Err(AgyError::Invalid(
                "AGY 설정이 변경되었습니다. 현재 상태를 읽은 뒤 다시 요청해 주세요.".into(),
            ));
        }
        if !matches!(command, Command::Off { .. }) && self.quiescent() {
            self.handle
                .ownership
                .worker()?
                .allow_recovery_after_quiescence()?;
            if self.writer.is_none() {
                let path = self.resources.settings_path.clone();
                self.writer = Some(
                    self.handle
                        .ownership
                        .worker()?
                        .call(move || Configuration::acquire(&path))
                        .await?,
                );
            }
        }
        if matches!(command, Command::Recheck { .. })
            && self.view.can_configure(self.quiescent())
            && let Some(writer) = self.writer.clone()
            && let Some(settings) = self
                .handle
                .ownership
                .worker()?
                .call(move || writer.reload())
                .await?
        {
            self.cache(settings);
        }
        match command {
            Command::On { expected_revision } | Command::Recheck { expected_revision } => {
                if matches!(self.view.phase, Phase::Checking | Phase::Provisioning) {
                    return Ok(());
                }
                if !self.view.can_configure(self.quiescent()) {
                    return Err(AgyError::Invalid("AGY 정리 완료 후 다시 켜 주세요.".into()));
                }
                self.handle
                    .ownership
                    .refresh(&self.resources.mirror_db)
                    .await?;
                self.cleanup_started = None;
                let writer = self.writer.as_ref().ok_or_else(|| {
                    AgyError::Invalid("AGY 설정 작성 권한을 확인할 수 없습니다.".into())
                })?;
                let saved = writer
                    .save_async(
                        &self.handle.ownership.worker()?,
                        expected_revision,
                        None,
                        Some(true),
                    )
                    .await?;
                self.cache(saved);
                self.begin_activation()
            }
            Command::Configure {
                expected_revision,
                config,
            } => {
                if !self.view.can_configure(self.quiescent()) {
                    return Err(AgyError::Invalid(
                        "AGY를 끄고 정리가 끝난 뒤 설정을 바꿔 주세요.".into(),
                    ));
                }
                let writer = self.writer.as_ref().ok_or_else(|| {
                    AgyError::Invalid("AGY 설정 작성 권한을 확인할 수 없습니다.".into())
                })?;
                let saved = writer
                    .save_async(
                        &self.handle.ownership.worker()?,
                        expected_revision,
                        Some(config),
                        Some(false),
                    )
                    .await?;
                self.cache(saved);
                self.view.transition(
                    Phase::Off,
                    "설정 저장 완료. 켜기를 선택하면 인증을 확인합니다.",
                );
                Ok(())
            }
            Command::Off {
                expected_revision,
                immediate,
            } => {
                self.cancel
                    .store(true, std::sync::atomic::Ordering::Release);
                if immediate {
                    self.cleanup_started = Some(tokio::time::Instant::now());
                }
                if let Some(service) = self.handle.service() {
                    service.close_admission().await;
                    if immediate {
                        service.stop_all()?;
                    }
                }
                self.view.generation = self.view.generation.saturating_add(1);
                self.view.enabled = false;
                self.view
                    .transition(Phase::Draining, "새 접수를 닫고 기존 작업을 정리 중입니다.");
                if immediate && let Some(worker) = &self.worker {
                    let _ = worker.stop.send(true);
                }
                if immediate
                    && self.worker.is_none()
                    && self.cleaning.is_none()
                    && let Some(service) = self.handle.service()
                {
                    self.cleaning = Some(tokio::spawn(async move {
                        service.processes.cleanup_all().await
                    }));
                }
                let result = if let Some(writer) = &self.writer {
                    writer
                        .save_async(
                            &self.handle.ownership.worker()?,
                            expected_revision,
                            None,
                            Some(false),
                        )
                        .await
                        .map(|s| self.cache(s))
                } else {
                    Err(AgyError::Invalid(
                        "끄기 의도를 저장하지 못했습니다. 재시작 시 다시 활성화될 수 있습니다."
                            .into(),
                    ))
                };
                if let Err(error) = &result {
                    self.view.detail = format!(
                        "접수는 닫혔지만 끄기 저장 실패: {error}. 재시작 시 다시 켜질 수 있습니다."
                    );
                }
                result
            }
            Command::Login { .. } => self.login().await,
        }
    }
    #[allow(
        clippy::too_many_lines,
        reason = "one cohesive lifecycle or contract scenario preserves ordering visibly"
    )]
    async fn advance(&mut self) {
        if self.view.phase == Phase::Checking
            && self.provisioning.load(std::sync::atomic::Ordering::Acquire)
        {
            self.view
                .transition(Phase::Provisioning, "AGY 방 연결을 확인 중입니다.");
            self.publish();
        }
        if self.login.active() {
            let stopping = self.cancel.load(std::sync::atomic::Ordering::Acquire);
            match self.login.poll(stopping) {
                Ok(true) if !stopping => {
                    self.view.transition(Phase::VerificationRequired,"로그인 창이 종료되었습니다. 안전한 인증 조회가 확인되기 전에는 활성화하지 않습니다.");
                    self.publish();
                }
                Err(error) => {
                    self.view
                        .transition(Phase::FaultedBlocked, error.to_string());
                    self.publish();
                }
                _ => {}
            }
        }
        if self
            .activation
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            let task = self.activation.take().expect("finished activation");
            match task.await {
                Ok((generation, Ok(Activation::Blocked(phase, detail))))
                    if generation == self.view.generation =>
                {
                    self.view.transition(phase, detail);
                }
                Ok((generation, Ok(Activation::Prepared(prepared, cutoff)))) => {
                    let current = generation == self.view.generation && self.view.enabled;
                    let service = Arc::clone(&prepared.service);
                    let (stop, shutdown) = watch::channel(!current);
                    if current {
                        if let Err(error) = service
                            .open_admission(generation, self.resources.runtime_uuid.clone(), cutoff)
                            .await
                        {
                            self.view
                                .transition(Phase::FaultedCleaning, error.to_string());
                            let _ = stop.send(true);
                        } else {
                            self.view.transition(Phase::Ready, "AGY 준비 완료");
                        }
                    }
                    if let Ok(mut slot) = self.handle.service.write() {
                        *slot = Some(service);
                    }
                    self.worker = Some(Worker {
                        stop,
                        task: tokio::spawn(prepared.run(shutdown)),
                    });
                }
                Ok((generation, Err(error))) if generation == self.view.generation => self
                    .view
                    .transition(Phase::FaultedCleaning, error.to_string()),
                Err(_) => self.view.transition(
                    Phase::FaultedCleaning,
                    "AGY 초기화 작업이 비정상 종료되었습니다.",
                ),
                _ => {}
            }
            self.publish();
        }
        if matches!(self.view.phase, Phase::Draining | Phase::FaultedCleaning)
            && let Some(service) = self.handle.service()
        {
            service.close_admission().await;
            if service.quiescent()
                && let Some(worker) = &self.worker
            {
                let _ = worker.stop.send(true);
            }
        }
        if self.worker.as_ref().is_some_and(|w| w.task.is_finished()) {
            let worker = self.worker.take().expect("finished worker");
            match worker.task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => self
                    .view
                    .transition(Phase::FaultedCleaning, error.to_string()),
                Err(_) => self.view.transition(
                    Phase::FaultedCleaning,
                    "AGY 실행 관리자가 비정상 종료되었습니다.",
                ),
            }
            if let Some(service) = self.handle.service() {
                service.close_admission().await;
                self.cleaning = Some(tokio::spawn(async move {
                    service.processes.cleanup_all().await
                }));
            }
            self.publish();
        }
        if self.cleaning.as_ref().is_some_and(JoinHandle::is_finished) {
            let result = self.cleaning.take().expect("finished cleanup").await;
            if !matches!(result, Ok(Ok(()))) {
                self.view.transition(
                    Phase::FaultedBlocked,
                    "AGY 소유 프로세스 종료 미확인. 다시 켜기를 보류합니다.",
                );
            }
            self.publish();
        }
        if self.worker.is_none()
            && self.cleaning.is_none()
            && let Ok(mut slot) = self.handle.service.write()
            && slot.as_ref().is_some_and(|s| s.quiescent())
        {
            *slot = None;
        }
        if self
            .cleanup_started
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(5))
            && !self.quiescent()
            && matches!(self.view.phase, Phase::Draining | Phase::FaultedCleaning)
        {
            self.view.transition(Phase::FaultedBlocked,"AGY 정리가 5초 안에 확인되지 않았습니다. 소유 작업 정리 완료 전 다시 켜기를 보류합니다.");
            self.publish();
        }
        if matches!(self.view.phase, Phase::Draining | Phase::FaultedCleaning) && self.quiescent() {
            let phase = if self.view.phase == Phase::Draining {
                Phase::Off
            } else {
                Phase::Faulted
            };
            self.view.transition(phase, self.view.detail.clone());
            self.publish();
        }
    }
    async fn login(&mut self) -> Result<()> {
        if !self.view.can_configure(self.quiescent()) {
            return Err(AgyError::Invalid(
                "AGY를 끄고 모든 작업 정리가 끝난 뒤 로그인해 주세요.".into(),
            ));
        }
        let settings = self.handle.settings()?.ok_or_else(|| {
            AgyError::Invalid("먼저 AGY 실행 파일과 프로젝트를 설정해 주세요.".into())
        })?;
        self.cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.login
            .launch(settings.config, &self.handle.ownership.worker()?)
            .await?;
        self.view.transition(
            Phase::Checking,
            "공식 AGY 로그인 창에서 로그인한 후 창을 닫아 주세요.",
        );
        Ok(())
    }
}

pub(super) type ProbeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<activation::Evidence>> + Send + 'a>>;

#[cfg(test)]
pub(crate) mod tests;
