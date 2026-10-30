use super::*;
use crate::agy::{
    lifecycle::Phase,
    manager::tests::routing_handle,
    tests::support::{fixture, message},
};
use cdr_discord::gateway::ingress::{GatewayIngress, GatewayIngressConfig};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::time::Instant;
use twilight_model::gateway::{
    event::Event,
    payload::incoming::{InteractionCreate, MessageCreate},
};
struct HeldMessages {
    calls: Arc<AtomicUsize>,
    until: Instant,
}
impl Messenger for HeldMessages {
    fn send<'a>(&'a self, _: u64, _: &'a str, _: &'a str) -> crate::agy::transport::SendFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            tokio::time::sleep_until(self.until).await;
            Ok(())
        })
    }
}
fn interaction(id: u64) -> InteractionCreate {
    serde_json::from_value(serde_json::json!({"application_id":"2","authorizing_integration_owners":{},"id":id.to_string(),"token":"fixture","type":2,"guild_id":"1","channel_id":"90","user":{"id":"7","username":"fixture","discriminator":"0001","avatar":null},"data":{"id":"1","name":if id.is_multiple_of(4) {"stop"}else{"status"},"type":1}})).unwrap()
}

#[tokio::test]
async fn lookup_errors_panics_and_history_failure_stay_inside_agy_lanes() {
    for behavior in [1, 2, 0] {
        let fixture = fixture().await;
        fixture.remote.behavior.store(behavior, Ordering::SeqCst);
        let (manager, _status, mut failures) = routing_handle(
            Some(Arc::clone(&fixture.service)),
            Arc::clone(&fixture.service.ownership),
            Phase::Ready,
        );
        let (stop, rx) = watch::channel(false);
        let lanes_owner = AgyRouting::start_with_adapters(
            manager,
            Arc::clone(&fixture.service.policy),
            Arc::new(FakeInteractions::default()),
            rx,
            Arc::clone(&fixture.service.remote),
            fixture.messages.clone(),
        );
        let (gateway, mut lanes) = GatewayIngress::new(GatewayIngressConfig {
            message_capacity: 1,
            ..Default::default()
        })
        .unwrap();
        let gaps = gateway.subscribe_message_gaps();
        if behavior == 0 {
            for id in [1, 2] {
                let _ = gateway.publish(
                    Event::MessageCreate(Box::new(MessageCreate(message(
                        id,
                        fixture.thread,
                        "gap",
                    )))),
                    Instant::now(),
                );
            }
            let notice = gaps.snapshot().unwrap().pop().unwrap();
            assert!(lanes_owner.routing.gap(notice, &gaps, 9).is_none());
            lanes.messages.recv().await.unwrap();
        } else {
            let _ = gateway.publish(
                Event::MessageCreate(Box::new(MessageCreate(message(1, 999, "lookup")))),
                Instant::now(),
            );
            let (resume, _) = mpsc::channel(1);
            assert!(
                lanes_owner
                    .routing
                    .message(lanes.messages.recv().await.unwrap(), 9, resume)
                    .is_none()
            );
        }
        tokio::time::timeout(Duration::from_millis(500), failures.recv())
            .await
            .unwrap()
            .unwrap();
        if behavior == 0 {
            assert_eq!(
                gaps.snapshot().unwrap().len(),
                1,
                "failed history must retain recovery notice"
            );
        }
        assert!(lanes_owner.tasks.iter().all(|task| !task.is_finished()));
        let (samples, mut received) = mpsc::unbounded_channel();
        let (done, done_rx) = watch::channel(false);
        let codex = tokio::spawn(crate::discord_runtime::run_agy_performance_lane(
            lanes.normal_interactions,
            Some(lanes_owner.routing.clone()),
            samples,
            done_rx,
        ));
        let _ = gateway.publish(
            Event::InteractionCreate(Box::new(interaction(2000))),
            Instant::now(),
        );
        tokio::time::timeout(Duration::from_millis(250), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fixture.remote.codex_reads.load(Ordering::SeqCst), 0);
        assert!(fixture.service.quiescent());
        done.send(true).unwrap();
        codex.await.unwrap().unwrap();
        stop.send(true).unwrap();
        lanes_owner
            .join(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }
}

#[tokio::test]
#[allow(
    deprecated,
    reason = "fixture exercises old Discord channel_id payload"
)]
async fn expired_interaction_cannot_resolve_rooms_or_accept_execution() {
    let fixture = fixture().await;
    let mut event = interaction(3000);
    event.0.channel_id = Some(twilight_model::id::Id::new(fixture.thread));
    let before = fixture.remote.reads.load(Ordering::SeqCst);
    assert!(
        crate::agy::ingress::acknowledge(
            &FakeInteractions::default(),
            &event.0,
            &cdr_discord::responses::autocomplete_response(),
            Instant::now() - Duration::from_millis(1),
        )
        .await
        .is_err()
    );
    assert_eq!(fixture.remote.reads.load(Ordering::SeqCst), before);
    assert!(fixture.service.quiescent());
}
#[derive(Clone, Debug, serde::Serialize)]
struct Measurement {
    mode: String,
    run: usize,
    admit_p95: f64,
    admit_max: f64,
    ack_p95: f64,
    ack_max: f64,
    stop_p95: f64,
    stop_max: f64,
    first_p95: f64,
    first_max: f64,
    agy_sends: usize,
    samples: Vec<(u64, usize, f64)>,
}
fn stats(values: &[f64]) -> (f64, f64) {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    (
        sorted[(sorted.len() * 95).div_ceil(100) - 1],
        *sorted.last().unwrap(),
    )
}
#[allow(
    clippy::too_many_lines,
    reason = "one cohesive lifecycle or contract scenario preserves ordering visibly"
)]
async fn measure(mode: &str, run: usize) -> Measurement {
    let mut fixture = fixture().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let until = Instant::now() + Duration::from_secs(5);
    let held: Arc<dyn Messenger> = Arc::new(HeldMessages {
        calls: Arc::clone(&calls),
        until,
    });
    Arc::get_mut(&mut fixture.service).unwrap().messenger = Arc::clone(&held);
    let phase = match mode {
        "ready" | "lookup_delayed" | "db_locked" => Phase::Ready,
        "faulted" => Phase::Faulted,
        _ => Phase::Off,
    };
    let service = (phase == Phase::Ready).then(|| Arc::clone(&fixture.service));
    let database_lock = if mode == "db_locked" {
        let path = fixture.temp.path().join(".codex_discord_agy.sqlite");
        let (entered, started) = tokio::sync::oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            let db = rusqlite::Connection::open(path).unwrap();
            db.execute_batch("BEGIN EXCLUSIVE").unwrap();
            entered.send(()).unwrap();
            std::thread::sleep(Duration::from_secs(5));
            db.execute_batch("COMMIT").unwrap();
        });
        started.await.unwrap();
        Some(task)
    } else {
        None
    };
    if mode == "lookup_delayed" {
        fixture.remote.behavior.store(3, Ordering::SeqCst);
    }
    let (manager, _status, _failures) =
        routing_handle(service, Arc::clone(&fixture.service.ownership), phase);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let routing_tasks = AgyRouting::start_with_adapters(
        manager,
        Arc::clone(&fixture.service.policy),
        Arc::new(FakeInteractions::default()),
        shutdown_rx,
        Arc::clone(&fixture.service.remote),
        held,
    );
    let routing = (mode != "baseline").then(|| routing_tasks.routing.clone());
    let (gateway, mut lanes) = GatewayIngress::new(GatewayIngressConfig::default()).unwrap();
    let (resume, mut resolved) = mpsc::channel(32);
    let stamps = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let (samples, mut received) = mpsc::unbounded_channel();
    let (done, mut done_rx) = watch::channel(false);
    let router = routing.clone();
    let messages = samples.clone();
    let message_stamps = Arc::clone(&stamps);
    let message_task = tokio::spawn(async move {
        loop {
            let item = tokio::select! {_=done_rx.changed()=>break,item=lanes.messages.recv()=>item,item=resolved.recv()=>item};
            let Some(item) = item else {
                break;
            };
            let item = if let Some(router) = &router {
                let Some(item) = router.message(item, 9, resume.clone()) else {
                    continue;
                };
                item
            } else {
                item
            };
            // Fake Codex admission/first reply, after the same production routing boundary.
            let at: Instant = message_stamps.lock().unwrap()[&item.event.id.get()];
            let delay = at.elapsed().as_secs_f64() * 1000.0;
            messages.send((item.event.id.get(), 0, delay)).unwrap();
            messages
                .send((item.event.id.get(), 3, at.elapsed().as_secs_f64() * 1000.0))
                .unwrap();
        }
    });
    let interaction_task = tokio::spawn(crate::discord_runtime::run_agy_performance_lane(
        lanes.normal_interactions,
        routing.clone(),
        samples,
        done.subscribe(),
    ));
    for index in 0..20 {
        stamps.lock().unwrap().insert(index + 1, Instant::now());
        let _ = gateway.publish(
            Event::MessageCreate(Box::new(MessageCreate(message(index + 1, 90, "warmup")))),
            Instant::now(),
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Bounded AGY saturation includes 128 independent messages and five-second sends.
    if mode != "baseline" {
        if mode == "lookup_delayed" {
            let _ = gateway.publish(
                Event::MessageCreate(Box::new(MessageCreate(message(
                    999,
                    999,
                    "unknown agy ancestry",
                )))),
                Instant::now(),
            );
        }
        for index in 0..128 {
            let _ = gateway.publish(
                Event::MessageCreate(Box::new(MessageCreate(message(
                    1000 + index,
                    fixture.thread,
                    "agy",
                )))),
                Instant::now(),
            );
        }
    }
    let started = Instant::now();
    for index in 0..240 {
        if index % 6 == 0 {
            let _ = gateway.publish(
                Event::InteractionCreate(Box::new(interaction(2000 + index / 6))),
                Instant::now(),
            );
        } else {
            stamps.lock().unwrap().insert(3000 + index, Instant::now());
            let _ = gateway.publish(
                Event::MessageCreate(Box::new(MessageCreate(message(3000 + index, 90, "codex")))),
                Instant::now(),
            );
        }
        tokio::time::sleep_until(started + Duration::from_millis((index + 1) * 5)).await;
    }
    let mut values = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    let mut trace = Vec::new();
    tokio::time::timeout(Duration::from_millis(500), async {
        while values[0].len() < 200
            || values[1].len() < 40
            || values[2].len() < 10
            || values[3].len() < 200
        {
            let (id, kind, value) = received.recv().await.unwrap();
            if id >= 2000 {
                trace.push((id, kind, value));
                values[kind].push(value);
            }
        }
    })
    .await
    .expect("Codex must finish while AGY is still held");
    assert!(Instant::now() < until);
    assert!(routing_tasks.routing.normal.capacity() <= 32);
    assert_eq!(
        fixture.remote.codex_reads.load(Ordering::SeqCst),
        0,
        "known Codex/AGY rooms need no classifier REST calls"
    );
    let (admit_p95, admit_max) = stats(&values[0]);
    let (ack_p95, ack_max) = stats(&values[1]);
    let (stop_p95, stop_max) = stats(&values[2]);
    let (first_p95, first_max) = stats(&values[3]);
    assert!(admit_max <= 500.0 && first_max <= 500.0 && ack_max <= 2500.0 && stop_max <= 250.0);
    done.send(true).unwrap();
    message_task.await.unwrap();
    interaction_task.await.unwrap().unwrap();
    shutdown.send(true).unwrap();
    routing_tasks
        .join(until + Duration::from_secs(3))
        .await
        .unwrap();
    if let Some(task) = database_lock {
        task.await.unwrap();
    }
    Measurement {
        mode: mode.into(),
        run,
        admit_p95,
        admit_max,
        ack_p95,
        ack_max,
        stop_p95,
        stop_max,
        first_p95,
        first_max,
        agy_sends: calls.load(Ordering::SeqCst),
        samples: trace,
    }
}

#[tokio::test]
async fn locked_database_and_five_second_lookup_do_not_hold_codex_ingress() {
    for mode in ["lookup_delayed", "db_locked"] {
        let result = measure(mode, 0).await;
        assert!(result.agy_sends > 0);
    }
}
#[tokio::test]
async fn agy_gateway_route_performance_contract_five_interleaved_runs() {
    let mut measurements = Vec::new();
    for run in 0..5 {
        let baseline = measure("baseline", run).await;
        measurements.push(baseline.clone());
        for mode in if run % 2 == 0 {
            ["off", "ready", "faulted"]
        } else {
            ["faulted", "ready", "off"]
        } {
            let measured = measure(mode, run).await;
            for (value, base) in [
                (measured.admit_p95, baseline.admit_p95),
                (measured.ack_p95, baseline.ack_p95),
                (measured.stop_p95, baseline.stop_p95),
                (measured.first_p95, baseline.first_p95),
            ] {
                assert!(
                    value - base <= 5.0_f64.max(base * 0.05),
                    "{mode} run {run}: p95 {value} baseline {base}"
                );
            }
            assert!(measured.agy_sends > 0);
            measurements.push(measured);
        }
    }
    let report = serde_json::to_string_pretty(&measurements).unwrap();
    println!("AGY_ROUTE_PERFORMANCE {report}");
    if let Some(path) = std::env::var_os("CDR_AGY_PERFORMANCE_REPORT") {
        std::fs::write(path, report).unwrap();
    }
}

#[derive(Default)]
struct FakeInteractions {
    acknowledgments: std::sync::Mutex<
        Vec<(
            u64,
            Instant,
            twilight_model::http::interaction::InteractionResponseType,
        )>,
    >,
    updates: AtomicUsize,
    hold_until: Option<Instant>,
    fail_ack: bool,
}

mod question_input_contracts;
mod response_contracts;
mod revision_contracts;
impl Interactions for FakeInteractions {
    fn acknowledge<'a>(
        &'a self,
        event: &'a twilight_model::application::interaction::Interaction,
        response: &'a twilight_model::http::interaction::InteractionResponse,
    ) -> crate::agy::transport::SendFuture<'a> {
        Box::pin(async move {
            if self.fail_ack {
                return Err(AgyError::Discord("fake ACK failure".into()));
            }
            self.acknowledgments.lock().unwrap().push((
                event.id.get(),
                Instant::now(),
                response.kind,
            ));
            Ok(())
        })
    }
    fn update<'a>(
        &'a self,
        _: &'a twilight_model::application::interaction::Interaction,
        _: &'a str,
    ) -> crate::agy::transport::SendFuture<'a> {
        Box::pin(async move {
            self.updates.fetch_add(1, Ordering::SeqCst);
            if let Some(until) = self.hold_until {
                tokio::time::sleep_until(until).await;
            }
            Ok(())
        })
    }
}
