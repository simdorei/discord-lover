use std::sync::Arc;

use cdr_app_server::{AppServerConfig, AppServerError, ResidentAppServer};
use cdr_discord::gateway::GatewayRuntime;
use cdr_remote_agent::runner::RemoteAgentControl;
use cdr_remote_agent::status::RemoteAgentStatus;
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};

use crate::completion_worker::run_completion_worker;
use crate::config::RuntimeConfig;
use crate::dead_generation_recovery::RuntimeDeadGenerationFence;
use crate::interaction_worker::run_interaction_worker;
use crate::prompt_intake_worker::run_prompt_intake_recovery_worker_with_admission;
use crate::runtime_instance::{RuntimeInstanceGuard, run_heartbeat};
use crate::runtime_paths::{RuntimePaths, discover_inputs};
use crate::server_request_worker::run_server_request_worker;

mod bootstrap;
mod error;
mod gateway_loop;
mod message_create;
mod resume_recovery;
mod shutdown;
mod shutdown_deadline;
mod startup_notice;
mod typed_ingress;
mod worker_supervision;
mod workers;

use bootstrap::{build_executor, interaction_policy, load_autocomplete, prepare_storage};
pub use error::DiscordRuntimeError;
use gateway_loop::{GatewayLoopContext, GatewayLoopOutcome, run_gateway_loop};
use shutdown::{ShutdownCause, validate_drain_cleanup};
use shutdown_deadline::{
    HEARTBEAT_JOIN_RESERVE, NON_HEARTBEAT_RESERVE, RUNTIME_SHUTDOWN_TIMEOUT,
    complete_before_shutdown_deadline,
};
use typed_ingress::{InteractionResources, TypedIngressContext, TypedIngressWorkers};
use worker_supervision::{WorkerSet, exit_channel};
use workers::{
    prepare_remote_worker, spawn_unit_worker, start_remote_worker, start_session_mirror_worker,
};

const INTERACTION_QUEUE_CAPACITY: usize = 64;

#[cfg(test)]
pub(crate) use typed_ingress::agy_isolation_contract::assert_common_consumers_survive;
#[cfg(test)]
pub(crate) use typed_ingress::run_agy_probe_lane;

#[cfg(test)]
pub(crate) async fn run_agy_performance_lane(
    receiver: mpsc::Receiver<cdr_discord::gateway::ingress::InteractionIngress>,
    routing: Option<crate::agy::routing::AgyRouting>,
    samples: mpsc::UnboundedSender<(u64, usize, f64)>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), DiscordRuntimeError> {
    typed_ingress::run_agy_performance_lane(receiver, routing, samples, shutdown).await
}

fn request_remote_handoff_if_needed(paths: &RuntimePaths, control: Option<&RemoteAgentControl>) {
    if crate::operation_marker::restart_requested(&paths.root)
        && let Some(control) = control
    {
        control.request_restart_handoff();
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "startup and shutdown ordering is linear and guarded by source contracts"
)]
pub async fn run(
    config: RuntimeConfig,
    environment: &std::collections::BTreeMap<String, String>,
) -> Result<(), DiscordRuntimeError> {
    let root = std::env::current_dir().map_err(DiscordRuntimeError::CurrentDirectory)?;
    let inputs = discover_inputs(environment, root)?;
    let paths = RuntimePaths::resolve(environment, &inputs)?;
    let agy_settings = paths.root.join(
        environment
            .get("DISCORD_AGY_CONFIG")
            .filter(|p| !p.trim().is_empty())
            .map_or("codex-agy.json", String::as_str),
    );
    let _runtime_instance = RuntimeInstanceGuard::acquire(&paths.root)?;
    let drain_controller = Arc::new(
        crate::restart_readiness::drain_controller::RuntimeDrainController::initialize(
            &paths.root,
        )?,
    );
    let admission_gate = drain_controller.admission_gate();
    prepare_storage(&paths)?;
    let retirement = cdr_store::reserve_retirement::retire(&paths.mirror_db, &[])?;
    eprintln!("manual_reserve_retirement: {retirement:?}");

    let server = start_app_server(
        &paths,
        drain_controller.runtime_id(),
        config.startup_channel_id,
    )
    .await?;
    resume_recovery::recover(&server, &paths.mirror_db, drain_controller.runtime_id()).await;
    let held_ingress = cdr_store::ingress::recover_prior_runtime(
        &paths.mirror_db,
        drain_controller.runtime_id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(cdr_store::StoreError::from)?
            .as_secs_f64(),
    )?;
    if held_ingress != 0 {
        eprintln!("rust_ingress_startup_held: {held_ingress}");
    }
    let completion_receiver = server.subscribe_notifications();
    let server_request_receiver = server.subscribe_server_requests();
    let remote_status = RemoteAgentStatus::default();
    let (queue, executor) = build_executor(
        &paths,
        &config,
        Arc::clone(&server),
        remote_status.clone(),
        admission_gate.clone(),
    )
    .await?;
    let interaction_policy = Arc::new(interaction_policy(&config, executor.mirror_db())?);
    let prepared_remote =
        prepare_remote_worker(config.remote_mcp.clone(), remote_status, &paths.root).await?;
    let config = Arc::new(config);
    let mut gateway = GatewayRuntime::start_paused(
        config.bot_token.expose().to_owned(),
        config.enable_message_content,
    )
    .await?;
    let http = gateway.http();
    let autocomplete = Arc::new(load_autocomplete(&server).await?);
    let attachment_client = Arc::new(reqwest::Client::new());
    let (completion_shutdown, completion_shutdown_rx) = watch::channel(false);
    let agy = crate::agy::manager::AgyManager::start(
        crate::agy::manager::Resources {
            root: paths.root.clone(),
            mirror_db: paths.mirror_db.clone(),
            settings_path: agy_settings,
            policy: Arc::clone(&config),
            http: Arc::clone(&http),
            admission: admission_gate.clone(),
            runtime_uuid: drain_controller.runtime_id().to_owned(),
            topology: gateway.topology(),
            #[cfg(test)]
            remote: None,
        },
        completion_shutdown.subscribe(),
    )
    .await;
    executor
        .configure_mirror_with_ownership(
            Arc::clone(&http),
            config.guild_id,
            Arc::clone(&agy.handle.ownership),
        )
        .map_err(|error| DiscordRuntimeError::MirrorSyncConfiguration(error.to_string()))?;
    let agy_routing = crate::agy::routing::AgyRouting::start(
        Arc::clone(&agy.handle),
        Arc::clone(&config),
        Arc::clone(&http),
        completion_shutdown.subscribe(),
    );
    #[cfg(windows)]
    let agy_control = crate::agy::control::Control::start(
        Arc::clone(&agy.handle),
        paths.root.clone(),
        completion_shutdown.subscribe(),
    );
    let (heartbeat_shutdown, heartbeat_shutdown_rx) = watch::channel(false);
    let (worker_exit_notifier, mut worker_exits) = exit_channel();
    let (work_sender, work_receiver) = mpsc::channel(INTERACTION_QUEUE_CAPACITY);
    let interaction_resources = InteractionResources::new(
        interaction_policy,
        work_sender.clone(),
        Arc::clone(&autocomplete),
        admission_gate.clone(),
        paths.mirror_db.clone(),
    );
    let typed_workers = TypedIngressWorkers::start(
        &mut gateway,
        TypedIngressContext::new(
            Arc::clone(&config),
            Arc::clone(&http),
            Arc::clone(&executor),
            Arc::clone(&server),
            paths.attachment_dir.clone(),
            Arc::clone(&attachment_client),
            interaction_resources,
            admission_gate.clone(),
        )
        .with_agy(Some(agy_routing.routing.clone())),
        completion_shutdown.subscribe(),
        worker_exit_notifier.clone(),
    )
    .await?;
    let mut workers = WorkerSet::from_workers(typed_workers.into_workers());
    workers.push(spawn_unit_worker(
        "interaction",
        worker_exit_notifier.clone(),
        run_interaction_worker(
            work_receiver,
            Arc::clone(&executor),
            Arc::clone(&server),
            Arc::clone(&http),
        ),
    ));
    let (remote_worker, remote_control) = start_remote_worker(
        prepared_remote,
        completion_shutdown.subscribe(),
        worker_exit_notifier.clone(),
    );
    if let Some(remote_worker) = remote_worker {
        workers.push(remote_worker);
    }
    workers.push(spawn_unit_worker(
        "app-server-supervisor",
        worker_exit_notifier.clone(),
        Arc::clone(&server).run_restart_supervisor(completion_shutdown.subscribe()),
    ));
    let heartbeat = spawn_unit_worker(
        "heartbeat",
        worker_exit_notifier.clone(),
        run_heartbeat(
            RuntimeInstanceGuard::heartbeat_path(&paths.root),
            heartbeat_shutdown_rx,
        ),
    );
    workers.push(spawn_unit_worker(
        "completion",
        worker_exit_notifier.clone(),
        run_completion_worker(
            completion_receiver,
            Arc::clone(&server),
            Arc::clone(&queue),
            Arc::clone(&http),
            config.stream_commentary,
            config.app_server_history_read_timeout,
            completion_shutdown_rx,
        ),
    ));
    workers.push(spawn_unit_worker(
        "new-first-reply-verification",
        worker_exit_notifier.clone(),
        crate::new_reply_worker::run(
            paths.mirror_db.clone(),
            Arc::clone(&queue),
            Arc::clone(&http),
            completion_shutdown.subscribe(),
        ),
    ));
    workers.push(spawn_unit_worker(
        "stop-control",
        worker_exit_notifier.clone(),
        Arc::clone(&executor).run_stop_worker(completion_shutdown.subscribe()),
    ));
    workers.push(spawn_unit_worker(
        "prompt-intake-recovery",
        worker_exit_notifier.clone(),
        run_prompt_intake_recovery_worker_with_admission(
            Arc::clone(&executor),
            admission_gate,
            completion_shutdown.subscribe(),
        ),
    ));
    if let Some(session_mirror_worker) = start_session_mirror_worker(
        config.session_mirror,
        &paths,
        &http,
        completion_shutdown.subscribe(),
        worker_exit_notifier.clone(),
    ) {
        workers.push(session_mirror_worker);
    }
    workers.push(spawn_unit_worker(
        "server-request",
        worker_exit_notifier.clone(),
        run_server_request_worker(
            server_request_receiver,
            Arc::clone(&server),
            paths.mirror_db.clone(),
            config.startup_channel_id,
            Arc::clone(&http),
            completion_shutdown.subscribe(),
            crate::config::ApprovalPolicies {
                all: config.approval_policy,
                local_tools: config.local_tool_approval_policy,
            },
        ),
    ));
    drop(worker_exit_notifier);
    let shutdown_cause = tokio::select! {
        result = run_gateway_loop(GatewayLoopContext {
            operation_root: paths.root.clone(),
            drain: Arc::clone(&drain_controller),
            server: Arc::clone(&server),
            mirror_db: paths.mirror_db.clone(),
        }) => ShutdownCause::Control(result),
        worker = worker_exits.recv() => match worker {
            Some(worker) => ShutdownCause::Worker(worker),
            None => ShutdownCause::WorkerMonitorClosed,
        },
        shard = gateway.wait_for_shard_exit() => match shard {
            Some(shard) => ShutdownCause::GatewayShard(shard),
            None => ShutdownCause::GatewayMonitorClosed,
        },
    };
    let drain_key = match &shutdown_cause {
        ShutdownCause::Control(Ok(GatewayLoopOutcome::Drained(key))) => Some(key.clone()),
        _ => None,
    };
    shutdown_cause.record_before_cleanup();
    let shutdown_deadline = Instant::now() + RUNTIME_SHUTDOWN_TIMEOUT;
    let non_heartbeat_deadline = shutdown_deadline - NON_HEARTBEAT_RESERVE;
    let server_close_deadline = shutdown_deadline - HEARTBEAT_JOIN_RESERVE;
    let triggering_worker = shutdown_cause.non_heartbeat_worker();
    let triggering_shard = shutdown_cause.gateway_shard();
    request_remote_handoff_if_needed(&paths, remote_control.as_ref());
    gateway.begin_stopping();
    drop(work_sender);
    let _ = completion_shutdown.send(true);
    let (gateway_result, workers_result) = tokio::join!(
        gateway.shutdown_for_cause(non_heartbeat_deadline, triggering_shard),
        workers.shutdown_for_cause(non_heartbeat_deadline, triggering_worker),
    );
    let agy_result = agy_routing
        .join(non_heartbeat_deadline)
        .await
        .and(agy.join(non_heartbeat_deadline).await);
    #[cfg(windows)]
    let agy_result = agy_result.and(agy_control.join(non_heartbeat_deadline).await);
    let workers_result = workers_result.with_optional_cleanup(agy_result.map_err(Into::into));
    let close_result =
        complete_before_shutdown_deadline(server_close_deadline, "app-server", server.close())
            .await;
    if let Some(key) = drain_key {
        let drain_result = async {
            validate_drain_cleanup(gateway_result, workers_result, close_result)?;
            let transition = drain_controller.complete_drained_handshake(&key).await;
            if matches!(
                transition,
                crate::restart_readiness::drain_controller::DrainedTransition::Restart
            ) {
                request_remote_handoff_if_needed(&paths, remote_control.as_ref());
            }
            Ok::<(), DiscordRuntimeError>(())
        }
        .await;
        let _ = heartbeat_shutdown.send(true);
        let heartbeat_result = heartbeat
            .shutdown_until(Instant::now() + HEARTBEAT_JOIN_RESERVE)
            .await;
        return match drain_result {
            Ok(()) => heartbeat_result,
            Err(error) => {
                if let Err(heartbeat_error) = heartbeat_result {
                    eprintln!(
                        "secondary_runtime_shutdown_error component=heartbeat error={heartbeat_error}"
                    );
                }
                Err(error)
            }
        };
    }
    let _ = heartbeat_shutdown.send(true);
    let heartbeat_result = heartbeat.shutdown_until(shutdown_deadline).await;
    shutdown_cause.propagate(
        gateway_result,
        workers_result,
        heartbeat_result,
        close_result,
    )
}

async fn start_app_server(
    paths: &RuntimePaths,
    runtime_id: &str,
    startup_channel_id: Option<u64>,
) -> Result<Arc<ResidentAppServer>, AppServerError> {
    let config = AppServerConfig::new(&paths.codex_exe).with_environment([(
        "CODEX_HOME".to_owned(),
        paths.codex_home.to_string_lossy().into_owned(),
    )]);
    let fence = Arc::new(RuntimeDeadGenerationFence::new(
        paths.mirror_db.clone(),
        runtime_id.to_owned(),
        startup_channel_id,
    )?);
    let server =
        Arc::new(ResidentAppServer::start_with_dead_generation_fence(config, fence).await?);
    if let Err(error) = crate::idle_release::install(&server, &paths.mirror_db) {
        if let Err(cleanup) = server.close().await {
            return Err(AppServerError::StartupCleanup {
                primary: Box::new(error),
                cleanup: Box::new(cleanup),
            });
        }
        return Err(error);
    }
    Ok(server)
}
