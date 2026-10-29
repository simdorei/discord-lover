use super::{Active, AgyError, AgyRuntime, Job, Result, Room, rooms, runner};
use futures_util::FutureExt;
use std::sync::Arc;
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};
use twilight_model::channel::ChannelType;

#[cfg(test)]
pub(super) struct SubmitBarrier {
    pub point: u8,
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}
#[cfg(test)]
impl AgyRuntime {
    async fn submit_boundary(&self, point: u8) {
        if let Some(barrier) = &self.submit_barrier
            && barrier.point == point
        {
            barrier.entered.notify_one();
            barrier.release.notified().await;
        }
    }
}

pub(super) async fn run(
    service: Arc<AgyRuntime>,
    mut receiver: mpsc::Receiver<Job>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    let mut failure = None;
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            biased;
            _=shutdown.changed()=>break,
            Some(result)=tasks.join_next(),if !tasks.is_empty()=>{
                match result {Ok(Ok(()))=>{},Ok(Err(error))=>{failure=Some(error);break;},Err(_)=>{failure=Some(AgyError::Invalid("AGY execution task panicked".into()));break;}}
            }
            job=receiver.recv()=>{
                let Some(job)=job else {break;};
                let owner=Arc::clone(&service);
                tasks.spawn(async move {
                    std::panic::AssertUnwindSafe(perform(owner,job)).catch_unwind().await
                        .unwrap_or_else(|_|Err(AgyError::Invalid("AGY execution task panicked".into())))
                });
            }
        }
    }
    service.close_admission().await;
    receiver.close();
    if let Err(error) = service.stop_all() {
        failure = Some(error);
    }
    while let Ok(job) = receiver.try_recv() {
        if let Err(error) = service
            .store
            .finish(
                &job.id,
                "interrupted",
                "실행 전에 AGY가 중단되었습니다. 자동 재실행하지 않습니다.",
            )
            .await
        {
            failure = Some(error);
        }
        // Job::drop releases registration and admission even on DB failure.
    }
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failure = Some(error),
            Err(_) => failure = Some(AgyError::CleanupUnconfirmed),
        }
    }
    if let Err(error) = service.processes.cleanup_all().await {
        failure = Some(error);
    }
    failure.map_or(Ok(()), Err)
}

impl AgyRuntime {
    pub(super) async fn submit(
        &self,
        id: &str,
        origin: u64,
        room: Room,
        prompt: String,
        new: bool,
    ) -> Result<&'static str> {
        // Off takes this same lock. Timeout is uncertain, never rollback.
        let gate = self.acceptance.lock().await;
        let input_id = id
            .rsplit(':')
            .next()
            .and_then(|id| id.parse::<u64>().ok())
            .ok_or_else(|| AgyError::Invalid("Invalid AGY input identity".into()))?;
        if !gate.open || input_id <= gate.cutoff {
            return Ok(
                "AGY 접수가 꺼져 있거나 활성화 이전 입력입니다. 준비 완료 후 새 요청을 보내 주세요.",
            );
        }
        let queue_slot = self
            .sender
            .try_reserve()
            .map_err(|_| AgyError::Invalid("AGY 실행 대기열이 가득 찼거나 닫혔습니다.".into()))?;
        let permit = self.admission.try_enter()?;
        {
            let active = self.active()?;
            if active.len() >= 4
                || active
                    .values()
                    .any(|a| a.origin == origin || a.target == Some(origin))
            {
                return Ok(
                    "AGY 작업이 진행 중입니다. 완료 후 다시 보내거나 !stop 으로 중단해 주세요.",
                );
            }
        }
        let owned = id.to_owned();
        let generation = gate.generation;
        #[cfg(test)]
        self.submit_boundary(0).await;
        if !self
            .store
            .call(move |store| store.claim_generation(&owned, origin, generation))
            .await?
        {
            return Ok("");
        }
        #[cfg(test)]
        self.submit_boundary(1).await;
        let (stop, stop_rx) = watch::channel(false);
        self.active()?.insert(
            id.into(),
            Active {
                origin,
                target: None,
                stop,
            },
        );
        #[cfg(test)]
        self.submit_boundary(2).await;
        queue_slot.send(Job {
            id: id.into(),
            origin,
            room,
            prompt,
            new,
            stop: stop_rx,
            _permit: permit,
            active: Arc::clone(&self.active),
        });
        Ok("AGY 요청을 접수했습니다.")
    }
}

async fn perform(service: Arc<AgyRuntime>, job: Job) -> Result<()> {
    let mut target = job.origin;
    let outcome = execute(&service, &job, &mut target).await;
    let (state, text, execution_error) = match outcome {
        Ok(text) => ("completed", text, None),
        Err(error) => {
            if (target != job.origin || matches!(job.room, Room::Session(_)))
                && let Err(save) = service.store.block_unbound_session(target).await
            {
                eprintln!(
                    "agy_unbound_session_save_failed request={} error={save}",
                    job.id
                );
            }
            let state = if matches!(error, AgyError::Stopped) {
                "stopped"
            } else {
                "failed"
            };
            (state, format!("AGY: {error}"), Some(error))
        }
    };
    let saved = service.store.finish(&job.id, state, &text).await;
    let cleaned = service.processes.cleanup(&job.id).await;
    cleaned?;
    saved?;
    if let Err(error) = service
        .messenger
        .send(target, &format!("{}:result", job.id), &text)
        .await
    {
        let saved = service
            .store
            .finish(&job.id, "delivery_failed", &text)
            .await;
        eprintln!(
            "agy_response_delivery_failed request={} channel={target} error={error}",
            job.id
        );
        saved?;
    }
    if matches!(execution_error, Some(AgyError::CleanupUnconfirmed)) {
        return Err(AgyError::CleanupUnconfirmed);
    }
    Ok(())
}

async fn execute(service: &AgyRuntime, job: &Job, target: &mut u64) -> Result<String> {
    if *job.stop.borrow() {
        return Err(AgyError::Stopped);
    }
    let project_name = match &job.room {
        Room::Project(name) => name.clone(),
        Room::Session(channel) => {
            service
                .store
                .session(*channel)
                .await?
                .ok_or_else(|| AgyError::Invalid("AGY session missing".into()))?
                .project
        }
        Room::Unsupported => return Err(AgyError::Invalid("unconfigured AGY room".into())),
    };
    let project = service
        .config
        .projects
        .iter()
        .find(|p| p.name == project_name)
        .ok_or_else(|| AgyError::Invalid("AGY project is no longer configured".into()))?;
    let parent = service.rooms.projects[&project_name];
    if job.new || matches!(job.room, Room::Project(_)) {
        *target = create_thread(service, job, project, parent).await?;
    }
    let session = service
        .store
        .session(*target)
        .await?
        .ok_or_else(|| AgyError::Invalid("AGY session missing".into()))?;
    if session.blocked {
        return Err(AgyError::Invalid(
            "이전 실행의 대화 번호를 확인할 수 없습니다. 기존 대화는 자동 재생하지 않습니다."
                .into(),
        ));
    }
    rooms::unowned_async(&service.store, &service.mirror_db, *target).await?;
    let channel = service
        .remote
        .channel(*target)
        .await?
        .ok_or_else(|| AgyError::Invalid("AGY conversation channel is missing".into()))?;
    rooms::validate(
        &channel,
        service.guild,
        Some(parent),
        ChannelType::PublicThread,
    )?;
    if let Some(active) = service.active()?.get_mut(&job.id) {
        active.target = Some(*target);
    }
    service.store.running(&job.id, *target).await?;
    if *job.stop.borrow() {
        return Err(AgyError::Stopped);
    }
    if *target != job.origin {
        service
            .messenger
            .send(
                job.origin,
                &format!("{}:room", job.id),
                &format!("AGY 대화: <#{}>", *target),
            )
            .await?;
    }
    service
        .messenger
        .send(
            *target,
            &format!("{}:start", job.id),
            "AGY 실행 중… 중단하려면 !stop 을 보내 주세요.",
        )
        .await?;
    let database = service.store.clone();
    let target = *target;
    runner::execute(
        &service.config,
        &job.prompt,
        session.conversation.as_deref(),
        &session.cwd,
        job.stop.clone(),
        &service.processes,
        &job.id,
        move |id| {
            let database = database.clone();
            async move { database.save_conversation(target, &id).await }
        },
    )
    .await
}

pub(super) async fn create_thread(
    service: &AgyRuntime,
    job: &Job,
    project: &super::config::Project,
    parent: u64,
) -> Result<u64> {
    let _custody = Arc::clone(&service.ownership.coordination)
        .lock_owned()
        .await;
    let receipt = format!("thread:{}", job.id);
    service.ownership.creation_pending(&receipt)?;
    service
        .store
        .begin_container(&receipt, Some(parent), "PublicThread")
        .await?;
    let title: String = job
        .prompt
        .lines()
        .next()
        .unwrap_or("AGY")
        .chars()
        .filter(|c| !c.is_control())
        .take(90)
        .collect();
    let channel = service
        .remote
        .create(
            service.guild,
            Some(parent),
            ChannelType::PublicThread,
            &title,
            None,
        )
        .await?;
    service.ownership.remember(channel.id, channel.parent_id)?;
    service
        .store
        .confirm_container(&receipt, channel.id)
        .await?;
    rooms::validate(
        &channel,
        service.guild,
        Some(parent),
        ChannelType::PublicThread,
    )?;
    rooms::unowned_async(&service.store, &service.mirror_db, channel.id).await?;
    service
        .store
        .create_session(channel.id, &project.name, &project.cwd)
        .await?;
    if let Some(active) = service.active()?.get_mut(&job.id) {
        active.target = Some(channel.id);
    }
    service.rooms.remember_session(channel.id)?;
    service.ownership.observe_remote(&channel)?;
    service.ownership.creations_settled(&receipt)?;
    Ok(channel.id)
}
