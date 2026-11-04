//! One owner for state FIFO and a separate bounded, channel-serialized HTTP dispatcher.
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use cdr_store::completion_work::{self, Cursor, Source};
use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};

use super::{CompletionWorker, CompletionWorkerError};

#[cfg(test)]
mod contention_tests;
pub(super) mod lanes;
#[cfg(test)]
mod orphan_discovery_tests;
#[cfg(test)]
mod preflight_tests;
#[cfg(test)]
mod round_tests;
mod work;
#[cfg(test)]
mod writer_retry_tests;
use lanes::{Envelope, HTTP_SLOTS, Ready, STATE_SLOTS, StateWork};

#[derive(Clone)]
struct Scan {
    source: Source,
    cursor: Cursor,
    next: Instant,
    wake: bool,
    pending: VecDeque<completion_work::Entry>,
}

impl Scan {
    fn offer_pending(&mut self, ready: &mut Ready, active: &HashSet<String>) -> bool {
        while ready.state.len() + ready.http.len() < lanes::READY_CAP {
            let Some(entry) = self.pending.pop_front() else {
                return true;
            };
            ready.durable(entry, active);
        }
        self.pending.is_empty()
    }

    fn discover(
        &mut self,
        reader: &mut completion_work::MetadataReadRound<'_>,
        ready: &mut Ready,
        active: &HashSet<String>,
    ) -> cdr_store::Result<Option<PageMetadata>> {
        // Offer the preceding page before deciding whether another page is due.
        if !self.offer_pending(ready, active) {
            return Ok(None);
        }
        let now = Instant::now();
        let restart = self.cursor.finished;
        let cursor = if restart {
            if !self.wake && now < self.next {
                return Ok(None);
            }
            Cursor::default()
        } else {
            self.cursor.clone()
        };
        let completion_work::PageRead { cursor, page } = reader.page(self.source, &cursor)?;
        let negatives = if self.source == Source::AsyncOrphan {
            reader
                .unprovable_orphans(&page.entries)?
                .into_iter()
                .map(|index| page.entries[index].clone())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut metadata = PageMetadata {
            oversized_identity: page.oversized_identity,
            held_receipt_heads: page.held_receipt_heads,
            deferred: DeferredOrphans::default(),
        };
        self.cursor = cursor;
        if restart {
            self.wake = false;
        }
        self.pending = page.entries.into();
        let first_new = ready.state.len();
        let _ = self.offer_pending(ready, active);
        // Only this page's newly appended hints may consume its negative facts.
        // Pending entries carry no sidecar into a later read snapshot.
        for (index, item) in ready.state.iter().enumerate().skip(first_new) {
            if let StateWork::Durable(entry) = item
                && negatives.iter().any(|negative| negative == entry)
            {
                metadata.deferred.entries.push((index, entry.clone()));
            }
        }
        if self.cursor.finished {
            self.next = now
                + Duration::from_secs(
                    if matches!(self.source, Source::Queue | Source::AsyncOrphan) {
                        30
                    } else {
                        1
                    },
                );
        }
        Ok(Some(metadata))
    }
}

#[derive(Clone)]
struct PageMetadata {
    oversized_identity: bool,
    held_receipt_heads: i64,
    deferred: DeferredOrphans,
}

#[derive(Clone, Default)]
struct DeferredOrphans {
    entries: Vec<(usize, completion_work::Entry)>,
}

impl DeferredOrphans {
    // Called synchronously after discovery returns, before any await or dispatch.
    // Inventory-only callers may ignore this sidecar and still see every hint.
    fn discard(self, ready: &mut Ready) {
        let mut index = 0;
        let before = ready.state.len();
        ready.state.retain(|item| {
            let skip = matches!(item, StateWork::Durable(entry)
                if entry.source == Source::AsyncOrphan
                    && self.entries.iter().any(|(position, original)| *position == index && original == entry));
            index += 1;
            !skip
        });
        let count = before - ready.state.len();
        if count > 0 {
            eprintln!(
                "completion_orphan_preflight_held count={count} reason=original_evidence_refused; evidence retained; no RPC scheduled"
            );
        }
    }
}

// Discovery only appends via Ready::durable. Existing live payloads/permits are
// never cloned or moved, and no await exposes speculative appended hints.
struct ReadyDraft<'a> {
    ready: &'a mut Ready,
    state_len: usize,
    http_len: usize,
    published: bool,
}

impl<'a> ReadyDraft<'a> {
    fn new(ready: &'a mut Ready) -> Self {
        Self {
            state_len: ready.state.len(),
            http_len: ready.http.len(),
            ready,
            published: false,
        }
    }

    fn publish(mut self) {
        self.published = true;
    }
}

impl Drop for ReadyDraft<'_> {
    fn drop(&mut self) {
        if !self.published {
            self.ready.state.truncate(self.state_len);
            self.ready.http.truncate(self.http_len);
        }
    }
}

type DiscoveryResult = (Source, cdr_store::Result<Option<PageMetadata>>);

fn read_metadata_round(
    scans: &mut [Scan],
    next: &mut usize,
    ready: &mut Ready,
    active: &HashSet<String>,
    identity: (&std::path::Path, &str, i64),
) -> cdr_store::Result<Vec<DiscoveryResult>> {
    if scans.is_empty() {
        return Ok(Vec::new());
    }
    let rotation = *next % scans.len();
    let mut staged = scans.to_vec();
    let ready_draft = ReadyDraft::new(ready);
    let (path, runtime, generation) = identity;
    let reports = completion_work::read_round(path, runtime, generation, |reader| {
        let mut reports = Vec::with_capacity(staged.len());
        for offset in 0..staged.len() {
            let index = (rotation + offset) % staged.len();
            let mut candidate = staged[index].clone();
            let source_ready = ReadyDraft::new(ready_draft.ready);
            let result = candidate.discover(reader, source_ready.ready, active);
            if result.is_ok() {
                staged[index] = candidate;
                source_ready.publish();
            }
            reports.push((staged[index].source, result));
        }
        reports
    })?;
    for (original, committed) in scans.iter_mut().zip(staged) {
        *original = committed;
    }
    *next = (rotation + 1) % scans.len();
    ready_draft.publish();
    Ok(reports)
}

type StateResult = (String, bool, bool, Result<(), CompletionWorkerError>);
type HttpResult = (i64, Source, Result<(), CompletionWorkerError>);

fn discover_round(
    scans: &mut [Scan],
    next: &mut usize,
    worker: &CompletionWorker,
    ready: &mut Ready,
    active: &HashSet<String>,
) -> DeferredOrphans {
    let result: Result<_, CompletionWorkerError> = (|| {
        let generation = i64::try_from(worker.server.generation())
            .map_err(|_| crate::queue_runner::QueueRunnerError::IntegerRange)?;
        Ok(read_metadata_round(
            scans,
            next,
            ready,
            active,
            (
                worker.queue.db_path(),
                worker.server.instance_id(),
                generation,
            ),
        )?)
    })();
    let reports = match result {
        Ok(reports) => reports,
        Err(error) => {
            worker.server.mark_idle_observation_gap();
            CompletionWorker::report(Err(error));
            return DeferredOrphans::default();
        }
    };
    let mut deferred = DeferredOrphans::default();
    for (source, report) in reports {
        match report {
            Ok(Some(page)) => {
                deferred.entries.extend(page.deferred.entries);
                if page.oversized_identity {
                    worker.server.mark_idle_observation_gap();
                    eprintln!(
                        "completion_metadata_over_budget source={source:?}; evidence retained"
                    );
                }
                if page.held_receipt_heads > 0 {
                    eprintln!(
                        "completion_receipt_heads_retained source={source:?} count={}; no retry scheduled",
                        page.held_receipt_heads,
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                worker.server.mark_idle_observation_gap();
                CompletionWorker::report(Err(error.into()));
            }
        }
    }
    deferred
}

pub(super) async fn run(worker: &CompletionWorker, mut pending: mpsc::Receiver<Envelope>) {
    let mut ready = Ready::default();
    let mut active = HashSet::new();
    let mut channels = HashMap::new();
    let mut native = 0;
    let mut states: FuturesUnordered<BoxFuture<'_, StateResult>> = FuturesUnordered::new();
    let mut http: FuturesUnordered<BoxFuture<'_, HttpResult>> = FuturesUnordered::new();
    let mut maintenance: FuturesUnordered<BoxFuture<'_, Result<(), CompletionWorkerError>>> =
        FuturesUnordered::new();
    let mut scans = Source::ALL.map(|source| Scan {
        source,
        cursor: Cursor::default(),
        next: Instant::now(),
        wake: false,
        pending: VecDeque::new(),
    });
    let mut tick = interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut maintenance_due = Instant::now();
    let mut next_scan = 0;
    let mut input_closed = false;
    let mut close_deadline = tokio::time::Instant::now();
    loop {
        // The upstream mpsc is also bounded (128); all retained live payloads
        // share one byte semaphore, including events in active futures.
        for _ in 0..128 {
            let Ok(event) = pending.try_recv() else {
                break;
            };
            admit(worker, &mut ready, event);
        }
        launch_state(worker, &mut ready, &mut active, &mut native, &mut states);
        launch_http(worker, &mut ready, &mut channels, &mut http);
        if maintenance.is_empty() && Instant::now() >= maintenance_due {
            maintenance.push(work::maintain(worker).boxed());
            maintenance_due = Instant::now() + Duration::from_secs(30);
        }
        if input_closed
            && ready.state.is_empty()
            && ready.http.is_empty()
            && states.is_empty()
            && http.is_empty()
            && maintenance.is_empty()
            && scans
                .iter()
                .all(|scan| scan.cursor.finished && !scan.wake && scan.pending.is_empty())
        {
            return;
        }
        tokio::select! {
            event = pending.recv(), if !input_closed => if let Some(event)=event {
                admit(worker,&mut ready,event);
            } else {
                input_closed=true;
                close_deadline=tokio::time::Instant::now()+Duration::from_secs(5);
            },
            Some((target,was_native,wake,result)) = states.next(), if !states.is_empty() => {
                if wake && result.is_ok() {
                    match fresh_heads(worker,&target) {
                        Ok(heads)=>{
                            for entry in heads.into_iter().rev() {
                                if !channels.values().any(|active:&completion_work::Entry|active.same_identity(&entry)) {
                                    ready.prioritize(entry);
                                }
                            }
                            for scan in &mut scans { if !scan.source.is_state() {scan.wake=true;} }
                        }
                        Err(error)=>CompletionWorker::report(Err(error)),
                    }
                }
                active.remove(&target);
                native -= usize::from(was_native);
                CompletionWorker::report(result);
            },
            Some((channel,source,result)) = http.next(), if !http.is_empty() => {
                if result.is_ok() {
                    for scan in &mut scans { if scan.source==source { scan.wake=true; } }
                }
                channels.remove(&channel);
                CompletionWorker::report(result);
            },
            Some(result) = maintenance.next(), if !maintenance.is_empty() => CompletionWorker::report(result),
            () = worker.queue.wait_for_delivery_ready() => {
                // Finish the current finite pass before scanning new rows.
                for scan in &mut scans { scan.wake = true; }
            },
            () = tokio::time::sleep_until(close_deadline), if input_closed => return,
            _ = tick.tick() => {
                match worker.queue.due_writer_retries() {
                    Ok(hints) => for hint in hints { ready.writer_retry(hint, &active); },
                    Err(error) => CompletionWorker::report(Err(error.into())),
                }
                discover_round(&mut scans,&mut next_scan,worker,&mut ready,&active)
                    .discard(&mut ready);
            },
        }
        // Confirmed receipts and SQLite-only work may all be immediately ready.
        // Yield this batch so the HTTP peer, socket driver and observer can run.
        tokio::task::yield_now().await;
    }
}

fn admit(worker: &CompletionWorker, ready: &mut Ready, event: Envelope) {
    if !ready.live(event) {
        worker.server.mark_idle_observation_gap();
        eprintln!("completion_ready_capacity_gap; durable evidence retained");
    }
}

fn launch_state<'a>(
    worker: &'a CompletionWorker,
    ready: &mut Ready,
    active: &mut HashSet<String>,
    native: &mut usize,
    futures: &mut FuturesUnordered<BoxFuture<'a, StateResult>>,
) {
    while futures.len() < STATE_SLOTS {
        let Some((item, admission, needs_native)) =
            ready.take_state_admitted(active, *native, |item| match work::prepare(worker, item) {
                Ok(admission) => admission,
                Err(error) => {
                    worker.server.mark_idle_observation_gap();
                    CompletionWorker::report(Err(error));
                    None
                }
            })
        else {
            break;
        };
        let target = item.target().to_owned();
        let wake = match &item {
            StateWork::Durable(_) | StateWork::WriterRetry(_) => true,
            StateWork::Live(event) => matches!(&event.event,
                cdr_app_server::ResidentNotificationEvent::Notification {notification,..}
                if matches!(notification.method.as_str(),
                    "turn/started"|"turn/completed"|"item/completed"|"thread/goal/updated")),
        };
        active.insert(target.clone());
        *native += usize::from(needs_native);
        futures.push(
            async move {
                let mode = super::Processing::Staged(&admission);
                let result = match item {
                    StateWork::Live(event) => worker.handle_mode(event.event, mode).await,
                    StateWork::Durable(entry) => work::state(worker, &entry, mode).await,
                    StateWork::WriterRetry(hint) => {
                        match admission.lease.consume_writer_retry(&hint) {
                            Ok(true) => worker.recover_lane(&hint.target, mode).await,
                            Ok(false) => Ok(()),
                            Err(error) => Err(error.into()),
                        }
                    }
                };
                (target, needs_native, wake, result)
            }
            .boxed(),
        );
    }
}

fn launch_http<'a>(
    worker: &'a CompletionWorker,
    ready: &mut Ready,
    active: &mut HashMap<i64, completion_work::Entry>,
    futures: &mut FuturesUnordered<BoxFuture<'a, HttpResult>>,
) {
    while futures.len() < HTTP_SLOTS {
        let Some(entry) = ready.take_http(active) else {
            break;
        };
        active.insert(entry.channel, entry.clone());
        futures.push(
            async move {
                (
                    entry.channel,
                    entry.source,
                    work::deliver(worker, &entry).await,
                )
            }
            .boxed(),
        );
    }
}

fn fresh_heads(
    worker: &CompletionWorker,
    target: &str,
) -> Result<Vec<completion_work::Entry>, CompletionWorkerError> {
    Ok(completion_work::heads_for_target(
        worker.queue.db_path(),
        target,
        worker.server.instance_id(),
        i64::try_from(worker.server.generation())
            .map_err(|_| crate::queue_runner::QueueRunnerError::IntegerRange)?,
    )?)
}
