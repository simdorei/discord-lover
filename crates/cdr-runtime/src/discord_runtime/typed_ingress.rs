use std::future::Future;

use cdr_discord::gateway::GatewayRuntime;
use tokio::sync::{oneshot, watch};

use super::DiscordRuntimeError;
use super::worker_supervision::{MonitoredWorker, WorkerExitNotifier, spawn_monitored};

#[cfg(test)]
pub(crate) mod agy_isolation_contract;
mod context;
mod history;
mod identity;
mod interaction;
#[cfg(test)]
pub(super) use interaction::run_agy_performance_lane;
#[cfg(test)]
pub(crate) use interaction::run_agy_probe_lane;
mod message;
mod ready;
mod receive_error;
mod spawn;

pub(super) use context::TypedIngressContext;
pub(super) use interaction::InteractionResources;
use spawn::spawn_consumers;

pub(super) struct TypedIngressWorkers {
    handles: Vec<MonitoredWorker>,
}

impl TypedIngressWorkers {
    pub(super) async fn start(
        gateway: &mut GatewayRuntime,
        context: TypedIngressContext,
        shutdown: watch::Receiver<bool>,
        exit_notifier: WorkerExitNotifier,
    ) -> Result<Self, DiscordRuntimeError> {
        let ready_identity = gateway.subscribe_identity();
        let ready_conflict = gateway.subscribe_identity_conflict();
        let message_identity = gateway.subscribe_identity();
        let message_conflict = gateway.subscribe_identity_conflict();
        let history_identity = gateway.subscribe_identity();
        let history_conflict = gateway.subscribe_identity_conflict();
        let message_gaps = gateway.subscribe_message_gaps();
        let receivers = gateway.take_ingress_receivers()?;

        let (handles, readiness) = spawn_consumers(
            receivers,
            context,
            shutdown,
            ready_identity,
            ready_conflict,
            message_identity,
            message_conflict,
            (
                gateway.subscribe_identity(),
                gateway.subscribe_identity_conflict(),
            ),
            history_identity,
            history_conflict,
            message_gaps,
            &exit_notifier,
        );
        for (lane, receiver) in readiness {
            if receiver.await.is_err() {
                abort_and_join(handles).await;
                return Err(DiscordRuntimeError::TypedIngressReadiness(lane));
            }
        }
        if let Err(error) = gateway.activate_typed_consumers() {
            abort_and_join(handles).await;
            return Err(error.into());
        }
        Ok(Self { handles })
    }

    pub(super) fn into_workers(self) -> Vec<MonitoredWorker> {
        self.handles
    }
}

fn spawn_announced<F>(
    lane: &'static str,
    ready: oneshot::Sender<()>,
    exit_notifier: WorkerExitNotifier,
    future: F,
) -> MonitoredWorker
where
    F: Future<Output = Result<(), DiscordRuntimeError>> + Send + 'static,
{
    spawn_monitored(lane, exit_notifier, async move {
        let _ = ready.send(());
        future.await
    })
}

pub(super) async fn abort_and_join(mut handles: Vec<MonitoredWorker>) {
    for worker in &handles {
        worker.abort();
    }
    for worker in &mut handles {
        let _ = worker.join().await;
    }
}
