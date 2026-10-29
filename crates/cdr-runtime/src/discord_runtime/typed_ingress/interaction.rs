use std::future::{Future, pending};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use cdr_discord::gateway::ingress::{InteractionIngress, InteractionIngressTag};
use cdr_discord::http::DiscordHttp;
use cdr_discord::interaction_access::InteractionAccessPolicy;
use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::{
    sync::{mpsc, watch},
    time::{Duration, Instant, sleep_until},
};
use twilight_http::Client;

use super::super::DiscordRuntimeError;
use super::TypedIngressContext;
use crate::discord_dispatch::{
    AutocompleteCatalog, InboundInteractionWork, InteractionClaimCache, InteractionDispatcher,
};
use crate::restart_readiness::drain::AdmissionGate;

const NORMAL_CONCURRENCY: usize = 16;
const RESERVED_CONCURRENCY: usize = 4;
const PENDING_CODEX_CAPACITY: usize = 64;
const OVERFLOW_ACK_CONCURRENCY: usize = 64;
const INTERACTION_CLAIM_CAPACITY: usize = 4_096;
const RESERVED_SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(13);

#[path = "interaction_failure.rs"]
mod failure;

type InteractionFuture =
    Pin<Box<dyn Future<Output = Result<(), DiscordRuntimeError>> + Send + 'static>>;

#[derive(Clone)]
pub(in crate::discord_runtime) struct InteractionResources {
    policy: Arc<InteractionAccessPolicy>,
    work: mpsc::Sender<InboundInteractionWork>,
    autocomplete: Arc<AutocompleteCatalog>,
    claims: InteractionClaimCache,
    admission: AdmissionGate,
    ingress_db: PathBuf,
}

impl InteractionResources {
    pub(in crate::discord_runtime) fn new(
        policy: Arc<InteractionAccessPolicy>,
        work: mpsc::Sender<InboundInteractionWork>,
        autocomplete: Arc<AutocompleteCatalog>,
        admission: AdmissionGate,
        ingress_db: PathBuf,
    ) -> Self {
        Self {
            policy,
            work,
            autocomplete,
            claims: InteractionClaimCache::new(INTERACTION_CLAIM_CAPACITY),
            admission,
            ingress_db,
        }
    }
}

#[derive(Clone)]
pub(super) struct DiscordInteractionHandler {
    http: Arc<Client>,
    qa_enabled: bool,
    resources: InteractionResources,
    settings_resolver: crate::settings_binding::SettingsTargetResolver,
    agy: Option<crate::agy::routing::AgyRouting>,
}

impl DiscordInteractionHandler {
    pub(super) fn from_context(context: &TypedIngressContext) -> Self {
        Self {
            http: Arc::clone(&context.http),
            qa_enabled: context.config.qa_commands,
            resources: context.interaction.clone(),
            settings_resolver: context.executor.settings_resolver(),
            agy: context.agy.clone(),
        }
    }
}

trait InteractionHandler: Clone + Send + Sync + 'static {
    fn handle(&self, item: InteractionIngress) -> InteractionFuture;
    fn reject(&self, mut item: InteractionIngress) -> InteractionFuture {
        item.tag = InteractionIngressTag::Busy;
        self.handle(item)
    }
    fn route(
        &self,
        item: InteractionIngress,
        _resume: mpsc::Sender<InteractionIngress>,
    ) -> Option<InteractionIngress> {
        Some(item)
    }
}

impl InteractionHandler for DiscordInteractionHandler {
    fn reject(&self, item: InteractionIngress) -> InteractionFuture {
        let http = Arc::clone(&self.http);
        Box::pin(async move {
            // Overflow may acknowledge only: no policy refresh, custody claim,
            // database access, or command execution in this bounded lane.
            let event = &item.event.0;
            let response = if event.kind == twilight_model::application::interaction::InteractionType::ApplicationCommandAutocomplete {
                cdr_discord::responses::autocomplete_response()
            } else {
                cdr_discord::responses::interaction_message(
                    "Codex Discord is busy. Request not accepted; please retry shortly.", true,
                )
            };
            let api = DiscordHttp::new(http, event.application_id);
            let deadline = item.received_at + Duration::from_millis(2500);
            if Instant::now() >= deadline {
                return Err(crate::discord_dispatch::DiscordDispatchError::Acknowledge(
                    "overflow acknowledgment expired; request not accepted".into(),
                )
                .into());
            }
            tokio::time::timeout_at(deadline, api.acknowledge(event.id, &event.token, &response))
                .await
                .map_err(|_| {
                    crate::discord_dispatch::DiscordDispatchError::Acknowledge(
                        "overflow acknowledgment deadline; request not accepted".into(),
                    )
                })?
                .map_err(|_| {
                    crate::discord_dispatch::DiscordDispatchError::Acknowledge(
                        "overflow acknowledgment failed; request not accepted".into(),
                    )
                })?;
            Ok(())
        })
    }
    fn route(
        &self,
        item: InteractionIngress,
        resume: mpsc::Sender<InteractionIngress>,
    ) -> Option<InteractionIngress> {
        if let Some(agy) = &self.agy {
            agy.interaction(item, resume)
        } else {
            Some(item)
        }
    }
    fn handle(&self, item: InteractionIngress) -> InteractionFuture {
        let handler = self.clone();
        Box::pin(async move {
            let api = Arc::new(DiscordHttp::new(
                Arc::clone(&handler.http),
                item.event.application_id,
            ));
            let mut policy = handler.resources.policy.as_ref().clone();
            crate::discord_runtime::bootstrap::refresh_mirror_policy(
                &mut policy,
                &handler.resources.ingress_db,
            )?;
            let dispatcher = InteractionDispatcher::new(
                api,
                Arc::new(policy),
                handler.qa_enabled,
                handler.resources.work.clone(),
                handler.resources.ingress_db.clone(),
            )
            .with_autocomplete_catalog(Arc::clone(&handler.resources.autocomplete))
            .with_claim_cache(handler.resources.claims.clone())
            .with_admission_gate(handler.resources.admission.clone());
            let dispatcher = dispatcher.with_settings_resolver(handler.settings_resolver.clone());
            let _ = dispatcher
                .dispatch(&item.event, item.received_at, item.tag)
                .await?;
            Ok(())
        })
    }
}

#[derive(Clone, Copy)]
enum InteractionLane {
    Normal,
    Reserved,
}

impl InteractionLane {
    const fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal-interaction",
            Self::Reserved => "reserved-interaction",
        }
    }

    const fn accepts(self, tag: InteractionIngressTag) -> bool {
        matches!(
            (self, tag),
            (Self::Normal, InteractionIngressTag::Normal)
                | (
                    Self::Reserved,
                    InteractionIngressTag::Busy | InteractionIngressTag::Stopping
                )
        )
    }

    fn shutdown_deadline(self) -> Option<Instant> {
        match self {
            Self::Normal => None,
            Self::Reserved => Some(Instant::now() + RESERVED_SHUTDOWN_DRAIN_TIMEOUT),
        }
    }
}

pub(super) async fn run_normal(
    receiver: mpsc::Receiver<InteractionIngress>,
    handler: DiscordInteractionHandler,
    shutdown: watch::Receiver<bool>,
) -> Result<(), DiscordRuntimeError> {
    run_lane(
        receiver,
        InteractionLane::Normal,
        NORMAL_CONCURRENCY,
        handler,
        shutdown,
    )
    .await
}

pub(super) async fn run_reserved(
    receiver: mpsc::Receiver<InteractionIngress>,
    handler: DiscordInteractionHandler,
    shutdown: watch::Receiver<bool>,
) -> Result<(), DiscordRuntimeError> {
    run_lane(
        receiver,
        InteractionLane::Reserved,
        RESERVED_CONCURRENCY,
        handler,
        shutdown,
    )
    .await
}

async fn run_lane<H: InteractionHandler>(
    mut receiver: mpsc::Receiver<InteractionIngress>,
    lane: InteractionLane,
    concurrency: usize,
    handler: H,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), DiscordRuntimeError> {
    let (resume, mut resolved) = mpsc::channel(32);
    let mut active = FuturesUnordered::new();
    let mut rejections = FuturesUnordered::new();
    let mut pending_codex = std::collections::VecDeque::<InteractionIngress>::new();
    let mut drain_deadline = None;
    let mut input_closed = false;
    loop {
        if drain_deadline.is_none() && *shutdown.borrow() {
            let Some(deadline) = lane.shutdown_deadline() else {
                return Ok(());
            };
            drain_deadline = Some(deadline);
        }
        while active.len() < concurrency {
            let Some(item) = pending_codex.pop_front() else {
                break;
            };
            let interaction_id = item.event.id.get();
            let token = item.event.token.clone();
            let processing = handler.handle(item);
            active.push(async move { (interaction_id, token, processing.await) });
        }
        if input_closed && active.is_empty() && pending_codex.is_empty() && rejections.is_empty() {
            return Ok(());
        }
        tokio::select! {
            biased;
            () = async {
                match drain_deadline {
                    Some(deadline) => sleep_until(deadline).await,
                    None => pending().await,
                }
            } => {
                return Err(DiscordRuntimeError::TypedIngressDrainTimeout(lane.name()));
            }
            changed = shutdown.changed(), if drain_deadline.is_none() => {
                if changed.is_err() || *shutdown.borrow() {
                    let Some(deadline) = lane.shutdown_deadline() else {
                        return Ok(());
                    };
                    drain_deadline = Some(deadline);
                }
            }
            result = active.next(), if !active.is_empty() => {
                let (id, token, result) = result.expect("non-empty interaction future set");
                failure::report_event_result(lane.name(), id, &token, result)?;
            }
            result = rejections.next(), if !rejections.is_empty() => {
                let (id, token, result): (u64, String, Result<(), DiscordRuntimeError>) = result.expect("non-empty rejection future set");
                failure::report_event_result(lane.name(), id, &token, result)?;
            }
            item = async {tokio::select! { item=receiver.recv()=>item,item=resolved.recv()=>item }}, if !input_closed => {
                let Some(item) = item else {
                    if drain_deadline.is_none() {
                        return Err(DiscordRuntimeError::TypedIngressClosed(lane.name()));
                    }
                    input_closed = true;
                    continue;
                };
                if !lane.accepts(item.tag) {
                    return Err(DiscordRuntimeError::TypedInteractionTag {
                        lane: lane.name(),
                        tag: item.tag,
                    });
                }
                let Some(item)=handler.route(item,resume.clone()) else {continue;};
                if pending_codex.len() < PENDING_CODEX_CAPACITY {
                    pending_codex.push_back(item);
                } else if rejections.len() < OVERFLOW_ACK_CONCURRENCY {
                    let id=item.event.id.get();
                    let token=item.event.token.clone();
                    let rejection=handler.reject(item);
                    rejections.push(async move {(id,token,rejection.await)});
                } else {
                    // Finite overload capacity is explicit, as at the gateway.
                    // Do not stop routing later AGY inputs or execute this one.
                    eprintln!("discord_interaction_hard_drop lane={} interaction={} reason=overflow_ack_capacity request_accepted=false",lane.name(),item.event.id);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "interaction_tests.rs"]
mod tests;

#[cfg(test)]
pub(in crate::discord_runtime) async fn run_agy_performance_lane(
    receiver: mpsc::Receiver<InteractionIngress>,
    routing: Option<crate::agy::routing::AgyRouting>,
    samples: mpsc::UnboundedSender<(u64, usize, f64)>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), DiscordRuntimeError> {
    run_agy_probe_lane(receiver, routing, samples, shutdown, None, false).await
}

#[cfg(test)]
pub(crate) async fn run_agy_probe_lane(
    receiver: mpsc::Receiver<InteractionIngress>,
    routing: Option<crate::agy::routing::AgyRouting>,
    samples: mpsc::UnboundedSender<(u64, usize, f64)>,
    shutdown: watch::Receiver<bool>,
    hold: Option<Arc<tokio::sync::Semaphore>>,
    reserved: bool,
) -> Result<(), DiscordRuntimeError> {
    #[derive(Clone)]
    struct FakeCodex {
        routing: Option<crate::agy::routing::AgyRouting>,
        samples: mpsc::UnboundedSender<(u64, usize, f64)>,
        hold: Option<Arc<tokio::sync::Semaphore>>,
    }
    impl InteractionHandler for FakeCodex {
        fn route(
            &self,
            item: InteractionIngress,
            resume: mpsc::Sender<InteractionIngress>,
        ) -> Option<InteractionIngress> {
            if let Some(routing) = &self.routing {
                routing.interaction(item, resume)
            } else {
                Some(item)
            }
        }
        fn handle(&self, item: InteractionIngress) -> InteractionFuture {
            let samples = self.samples.clone();
            let hold = self.hold.clone();
            Box::pin(async move {
                let id = item.event.id.get();
                let delay = item.received_at.elapsed().as_secs_f64() * 1000.0;
                samples.send((id, 1, delay)).unwrap();
                if let Some(hold) = hold {
                    hold.acquire().await.unwrap().forget();
                }
                if id.is_multiple_of(4) {
                    samples.send((id, 2, delay)).unwrap();
                }
                Ok(())
            })
        }
        fn reject(&self, item: InteractionIngress) -> InteractionFuture {
            let samples = self.samples.clone();
            Box::pin(async move {
                samples
                    .send((
                        item.event.id.get(),
                        9,
                        item.received_at.elapsed().as_secs_f64() * 1000.0,
                    ))
                    .unwrap();
                Ok(())
            })
        }
    }
    run_lane(
        receiver,
        if reserved {
            InteractionLane::Reserved
        } else {
            InteractionLane::Normal
        },
        if reserved {
            RESERVED_CONCURRENCY
        } else {
            NORMAL_CONCURRENCY
        },
        FakeCodex {
            routing,
            samples,
            hold,
        },
        shutdown,
    )
    .await
}

#[cfg(test)]
#[path = "interaction_failure_tests.rs"]
mod failure_tests;

#[cfg(test)]
#[path = "settings_rejection_tests.rs"]
mod settings_rejection_tests;
