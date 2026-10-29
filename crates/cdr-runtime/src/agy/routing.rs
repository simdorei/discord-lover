//! Shared ingress only reads custody and performs bounded `try_send`. All AGY
//! lookup, acknowledgments, persistence and response delivery run in these lanes.
use super::{
    AgyError, Result,
    ingress::Action,
    manager::{AgyManager, ServiceLease},
    ownership::Owner,
    transport::{DiscordInteractions, DiscordMessenger, Interactions, Messenger},
};
use crate::{
    config::RuntimeConfig,
    mirror_sync::{DiscordMirrorTransport, MirrorTransport},
};
use cdr_discord::gateway::ingress::{
    InteractionIngress, MessageGapNotice, MessageGapReceiver, MessageIngress,
};
use futures_util::FutureExt;
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};
use twilight_http::Client;
mod interaction;

struct MessageWork {
    envelope: MessageIngress,
    bot: u64,
    resume: mpsc::Sender<MessageIngress>,
    generation: Option<u64>,
    observed_generation: u64,
    control: bool,
}
struct InteractionWork {
    envelope: InteractionIngress,
    resume: mpsc::Sender<InteractionIngress>,
    generation: Option<u64>,
    observed_generation: u64,
}
enum Work {
    Message(MessageWork),
    Interaction(InteractionWork),
    Acknowledged(InteractionWork, ServiceLease, Action),
    Lookup(u64),
    Gap {
        notice: MessageGapNotice,
        gaps: MessageGapReceiver,
        bot: u64,
    },
    BusyMessage(MessageWork),
    BusyInteraction(InteractionWork),
}

#[cfg(test)]
mod tests;
#[derive(Clone)]
pub(crate) struct AgyRouting {
    manager: Arc<AgyManager>,
    policy: Arc<RuntimeConfig>,
    interactions: Arc<dyn Interactions>,
    remote: Arc<dyn MirrorTransport>,
    normal: mpsc::Sender<Work>,
    control: mpsc::Sender<Work>,
    lookup: mpsc::Sender<Work>,
    recovery: mpsc::Sender<Work>,
    acknowledgments: mpsc::Sender<Work>,
    rejections: mpsc::Sender<Work>,
    messenger: Arc<dyn Messenger>,
}
pub(crate) struct ManagedRouting {
    pub routing: AgyRouting,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl ManagedRouting {
    pub async fn join(self, deadline: tokio::time::Instant) -> Result<()> {
        for mut task in self.tasks {
            tokio::time::timeout_at(deadline, &mut task)
                .await
                .map_err(|_| AgyError::CleanupUnconfirmed)?
                .map_err(|_| AgyError::CleanupUnconfirmed)?;
        }
        Ok(())
    }
}
impl AgyRouting {
    pub fn start(
        manager: Arc<AgyManager>,
        policy: Arc<RuntimeConfig>,
        http: Arc<Client>,
        shutdown: watch::Receiver<bool>,
    ) -> ManagedRouting {
        Self::start_with_adapters(
            manager,
            policy,
            Arc::new(DiscordInteractions(Arc::clone(&http))),
            shutdown,
            Arc::new(DiscordMirrorTransport::new(Arc::clone(&http))),
            Arc::new(DiscordMessenger(http)),
        )
    }
    #[allow(
        clippy::needless_pass_by_value,
        reason = "this constructor takes lifecycle receiver ownership and distributes it to lanes"
    )]
    fn start_with_adapters(
        manager: Arc<AgyManager>,
        policy: Arc<RuntimeConfig>,
        interactions: Arc<dyn Interactions>,
        shutdown: watch::Receiver<bool>,
        remote: Arc<dyn MirrorTransport>,
        messenger: Arc<dyn Messenger>,
    ) -> ManagedRouting {
        let (normal, nrx) = mpsc::channel(32);
        let (control, crx) = mpsc::channel(16);
        let (lookup, lrx) = mpsc::channel(32);
        let (recovery, rrx) = mpsc::channel(8);
        let (acknowledgments, arx) = mpsc::channel(64);
        let (rejections, brx) = mpsc::channel(64);
        let service = Self {
            manager,
            policy,
            interactions,
            remote,
            normal,
            control,
            lookup,
            recovery,
            acknowledgments,
            rejections,
            messenger,
        };
        let mut tasks = Vec::new();
        for (receiver, limit) in [(nrx, 4), (crx, 4), (lrx, 2), (rrx, 1), (arx, 64), (brx, 64)] {
            let service = service.clone();
            let shutdown = shutdown.clone();
            tasks.push(tokio::spawn(async move {
                service.run_lane(receiver, limit, shutdown).await;
            }));
        }
        ManagedRouting {
            routing: service,
            tasks,
        }
    }
    pub fn owner(&self, id: u64) -> Owner {
        self.manager.ownership.classify(id)
    }
    pub(crate) fn ownership(&self) -> Arc<super::ownership::AgyOwnership> {
        Arc::clone(&self.manager.ownership)
    }
    fn generation(&self) -> Option<u64> {
        let status = self.manager.status();
        (status.phase == super::lifecycle::Phase::Ready).then_some(status.generation)
    }
    pub fn classify_later(&self, id: u64) {
        let _ = self.lookup.try_send(Work::Lookup(id));
    }
    fn allowed(&self, guild: Option<u64>, user: Option<u64>) -> bool {
        guild == self.policy.guild_id
            && user.is_some_and(|id| {
                self.policy.allowed_user_ids.is_empty()
                    || self.policy.allowed_user_ids.contains(&id)
            })
    }
    pub fn message(
        &self,
        envelope: MessageIngress,
        bot: u64,
        resume: mpsc::Sender<MessageIngress>,
    ) -> Option<MessageIngress> {
        let message = &envelope.event.0;
        let owner = self.owner(message.channel_id.get());
        if owner.permits_codex() {
            return Some(envelope);
        }
        if message.author.bot
            || message.webhook_id.is_some()
            || !self.policy.enable_message_content
            || !self.allowed(
                message.guild_id.map(twilight_model::id::Id::get),
                Some(message.author.id.get()),
            )
        {
            return None;
        }
        let control = message.content.trim_start().starts_with("!stop")
            || message.content.trim_start().starts_with("!status")
            || message.content.trim_start().starts_with("!help");
        let work = Work::Message(MessageWork {
            envelope,
            bot,
            resume,
            generation: self.generation(),
            observed_generation: self.manager.status().generation,
            control,
        });
        let lane = if owner == Owner::Uncertain {
            &self.lookup
        } else if control {
            &self.control
        } else {
            &self.normal
        };
        if let Err(error) = lane.try_send(work)
            && let Work::Message(item) = error.into_inner()
        {
            let _ = self.control.try_send(Work::BusyMessage(item));
        }
        None
    }
    #[allow(deprecated, reason = "older Discord interactions use channel_id")]
    pub fn interaction(
        &self,
        envelope: InteractionIngress,
        resume: mpsc::Sender<InteractionIngress>,
    ) -> Option<InteractionIngress> {
        let event = &envelope.event.0;
        let channel = event
            .channel
            .as_ref()
            .map(|c| c.id)
            .or(event.channel_id)
            .map(twilight_model::id::Id::get);
        let Some(channel) = channel else {
            return Some(envelope);
        };
        let owner = self.owner(channel);
        if owner.permits_codex() {
            return Some(envelope);
        }
        let work = Work::Interaction(InteractionWork {
            envelope,
            resume,
            generation: self.generation(),
            observed_generation: self.manager.status().generation,
        });
        if let Err(error) = self.acknowledgments.try_send(work)
            && let Work::Interaction(item) = error.into_inner()
            && self
                .rejections
                .try_send(Work::BusyInteraction(item))
                .is_err()
        {
            self.manager
                .fault("AGY acknowledgment capacity exhausted; no execution accepted".into());
        }
        None
    }
    pub fn gap(
        &self,
        notice: MessageGapNotice,
        gaps: &MessageGapReceiver,
        bot: u64,
    ) -> Option<MessageGapNotice> {
        let channel = notice.snapshot().channel_id.get();
        if self.owner(channel).permits_codex() {
            return Some(notice);
        }
        let _ = self.recovery.try_send(Work::Gap {
            notice,
            gaps: gaps.resubscribe(),
            bot,
        });
        None
    }
    async fn run_lane(
        self,
        mut receiver: mpsc::Receiver<Work>,
        limit: usize,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _=shutdown.changed()=>break,
                Some(result)=tasks.join_next(),if !tasks.is_empty()=>{
                    if !matches!(result,Ok(Ok(()))) {self.manager.fault("AGY 입력·전달 작업 실패. Codex 처리는 계속됩니다.".into());}
                }
                work=receiver.recv(),if tasks.len()<limit=>{
                    let Some(work)=work else {break;};let service=self.clone();
                    tasks.spawn(async move {std::panic::AssertUnwindSafe(service.process(work)).catch_unwind().await.unwrap_or_else(|_|Err(AgyError::Invalid("AGY ingress task panicked".into())))});
                }
            }
            if *shutdown.borrow() {
                break;
            }
        }
        receiver.close();
        // Only routing/delivery futures live here. Model execution is owned by the manager.
        while let Some(result) = tasks.join_next().await {
            if !matches!(result, Ok(Ok(()))) {
                self.manager.fault("AGY 입력 정리 실패".into());
            }
        }
    }
    async fn classify(&self, id: u64) -> Result<Owner> {
        tokio::time::timeout(
            Duration::from_secs(2),
            self.manager.ownership.verify(id, &self.remote),
        )
        .await
        .map_err(|_| AgyError::Invalid("AGY 소유권 조회 시간 초과".into()))?
    }
    async fn process(&self, work: Work) -> Result<()> {
        match work {
            Work::Lookup(id) => {
                self.classify(id).await?;
                Ok(())
            }
            Work::Message(item) => {
                let owner = self.classify(item.envelope.event.channel_id.get()).await?;
                if owner.permits_codex() {
                    item.resume.try_send(item.envelope).map_err(|_| {
                        AgyError::Invalid(
                            "Resolved message queue full; no execution accepted".into(),
                        )
                    })?;
                    return Ok(());
                }
                if owner == Owner::Agy
                    && item.observed_generation == self.manager.status().generation
                    && (item.control
                        || item.generation.is_some() && item.generation == self.generation())
                    && let Some(service) = self.manager.lease()
                {
                    if let Err(error) = service
                        .handle_message(&item.envelope.event.0, Some(item.bot))
                        .await
                    {
                        let _ = service
                            .report_message_failure(&item.envelope.event.0, &error)
                            .await;
                        return Err(error);
                    }
                    if service.store.worker.uncertain() {
                        return Err(AgyError::Invalid("AGY storage outcome uncertain".into()));
                    }
                    return Ok(());
                }
                self.message_status(&item, owner == Owner::Conflict).await
            }
            Work::Interaction(item) => self.process_interaction(item, false).await,
            Work::Acknowledged(item, service, action) => {
                // A queue delay or Off transition cannot retarget a control to a
                // new generation. The lease also prevents replacement recovery.
                if item.observed_generation != self.manager.status().generation
                    || self.owner(interaction::channel(&item.envelope)?) != Owner::Agy
                {
                    return self
                        .interactions
                        .update(
                            &item.envelope.event.0,
                            "AGY 상태가 변경되어 실행하지 않았습니다. 다시 요청해 주세요.",
                        )
                        .await;
                }
                service
                    .complete_interaction(
                        &item.envelope.event.0,
                        action,
                        self.interactions.as_ref(),
                    )
                    .await?;
                if service.store.worker.uncertain() {
                    return Err(AgyError::Invalid("AGY storage outcome uncertain".into()));
                }
                Ok(())
            }
            Work::BusyMessage(item) => self.message_status(&item, true).await,
            Work::BusyInteraction(item) => self.process_interaction(item, true).await,
            Work::Gap { notice, gaps, bot } => {
                let snapshot = notice.snapshot();
                let owner = self.classify(snapshot.channel_id.get()).await?;
                if owner != Owner::Agy {
                    return Ok(());
                }
                let covered = if let Some(service) = self.manager.lease() {
                    service
                        .recover_gap(
                            snapshot.channel_id.get(),
                            snapshot.earliest.message_id.get(),
                            bot,
                        )
                        .await?
                } else {
                    true
                };
                if covered {
                    gaps.acknowledge(notice)
                        .map_err(|e| AgyError::Invalid(e.to_string()))?;
                }
                Ok(())
            }
        }
    }
    async fn message_status(&self, item: &MessageWork, blocked: bool) -> Result<()> {
        let status = self.manager.status();
        let message = if blocked {
            "AGY 접수를 보류했습니다. 대기열 또는 방 소유권 상태를 확인해 주세요.".to_owned()
        } else {
            format!("AGY {:?}: {}", status.phase, status.detail)
        };
        self.messenger
            .send(
                item.envelope.event.channel_id.get(),
                &format!("message:{}:agy-status", item.envelope.event.id),
                &message,
            )
            .await
    }
}
