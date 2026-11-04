//! Fixed-capacity ready queues. No idle target or receipt retry map is retained.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use cdr_app_server::{ResidentNotificationEvent, extract_thread_id};
use cdr_store::completion_work::Entry;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(super) const READY_CAP: usize = 128;
pub(super) const TARGET_CAP: usize = 16;
pub(crate) const EVENT_BYTES: usize = 4 * 1024 * 1024;
pub(super) const STATE_SLOTS: usize = 4;
pub(super) const NATIVE_SLOTS: usize = 3;
pub(super) const HTTP_SLOTS: usize = 4;

pub(crate) struct Envelope {
    pub event: ResidentNotificationEvent,
    pub target: String,
    _bytes: OwnedSemaphorePermit,
}

impl Envelope {
    pub(crate) fn charge(
        event: ResidentNotificationEvent,
        budget: &Arc<Semaphore>,
    ) -> Option<Self> {
        let ResidentNotificationEvent::Notification { notification, .. } = &event else {
            return None;
        };
        let target = extract_thread_id(&notification.params)?;
        if target.len() > cdr_store::completion_work::MAX_METADATA_BYTES {
            return None;
        }
        let mut count = ByteCount(
            notification
                .method
                .len()
                .saturating_add(target.len())
                .max(1),
        );
        serde_json::to_writer(&mut count, &notification.params).ok()?;
        let bytes = u32::try_from(count.0).ok()?;
        let permit = Arc::clone(budget).try_acquire_many_owned(bytes).ok()?;
        Some(Self {
            event,
            target,
            _bytes: permit,
        })
    }
}

struct ByteCount(usize);
impl std::io::Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) enum StateWork {
    Live(Envelope),
    Durable(Entry),
    WriterRetry(crate::queue_runner::WriterRetryHint),
}

impl StateWork {
    pub fn target(&self) -> &str {
        match self {
            Self::Live(e) => &e.target,
            Self::Durable(e) => &e.target,
            Self::WriterRetry(hint) => &hint.target,
        }
    }

    pub fn needs_native(&self) -> bool {
        match self {
            Self::Durable(_) | Self::WriterRetry(_) => true,
            Self::Live(e) => match &e.event {
                ResidentNotificationEvent::Notification { notification, .. } => {
                    notification.method == "thread/goal/updated"
                        || (notification.method == "turn/completed"
                            && notification.params["turn"]["status"] == "completed")
                }
                ResidentNotificationEvent::Gap { .. } => false,
            },
        }
    }
}

#[derive(Default)]
pub(super) struct Ready {
    pub state: VecDeque<StateWork>,
    pub http: VecDeque<Entry>,
}

impl Ready {
    fn len(&self) -> usize {
        self.state.len() + self.http.len()
    }

    pub fn live(&mut self, event: Envelope) -> bool {
        // Queued recovery cannot overtake a start/handoff already accepted here.
        self.state
            .retain(|w| matches!(w, StateWork::Live(_)) || w.target() != event.target);
        if self
            .state
            .iter()
            .filter(|w| w.target() == event.target)
            .count()
            >= TARGET_CAP
        {
            return false;
        }
        if self.len() >= READY_CAP {
            if let Some(index) = self
                .state
                .iter()
                .position(|w| !matches!(w, StateWork::Live(_)))
            {
                self.state.remove(index);
            } else if self.http.pop_back().is_none() {
                return false;
            }
        }
        self.state.push_back(StateWork::Live(event));
        true
    }

    pub fn durable(&mut self, entry: Entry, active: &HashSet<String>) {
        if self.len() >= READY_CAP {
            return;
        }
        if entry.source.is_state() {
            if active.contains(&entry.target)
                || self.state.iter().any(|w| w.target() == entry.target)
            {
                return;
            }
            self.state.push_back(StateWork::Durable(entry));
        } else if !self.http.iter().any(|e| e.same_identity(&entry)) {
            self.http.push_back(entry);
        }
    }

    pub fn writer_retry(
        &mut self,
        hint: crate::queue_runner::WriterRetryHint,
        active: &HashSet<String>,
    ) {
        if self.len() < READY_CAP
            && !active.contains(&hint.target)
            && !self.state.iter().any(|work| work.target() == hint.target)
        {
            self.state.push_back(StateWork::WriterRetry(hint));
        }
    }

    pub fn prioritize(&mut self, entry: Entry) {
        self.http.retain(|prior| !prior.same_identity(&entry));
        if self.len() >= READY_CAP && self.http.pop_back().is_none() {
            let Some(index) = self
                .state
                .iter()
                .position(|w| !matches!(w, StateWork::Live(_)))
            else {
                return;
            };
            self.state.remove(index);
        }
        self.http.push_front(entry);
    }

    #[cfg(test)]
    pub fn take_state(&mut self, active: &HashSet<String>, native: usize) -> Option<StateWork> {
        self.take_state_admitted(active, native, |work| Some(((), work.needs_native())))
            .map(|(work, (), _)| work)
    }

    pub fn take_state_admitted<T>(
        &mut self,
        active: &HashSet<String>,
        native: usize,
        mut admit: impl FnMut(&StateWork) -> Option<(T, bool)>,
    ) -> Option<(StateWork, T, bool)> {
        let mut heads = HashSet::new();
        let mut deferred = Vec::new();
        let selected = self.state.iter().enumerate().find_map(|(index, work)| {
            // A blocked head blocks only its own target, never a later B head.
            if !heads.insert(work.target()) || active.contains(work.target()) {
                return None;
            }
            let Some((permit, needs_native)) = admit(work) else {
                // Only discard rediscoverable metadata, never a live event
                // or its durable DB evidence. Busy hints cannot own ready capacity.
                if !matches!(work, StateWork::Live(_)) {
                    deferred.push(index);
                }
                return None;
            };
            (!needs_native || native < NATIVE_SLOTS).then_some((index, permit, needs_native))
        });
        drop(heads);
        let chosen = selected.and_then(|(index, permit, needs_native)| {
            self.state
                .remove(index)
                .map(|work| (work, permit, needs_native))
        });
        // Every deferred index precedes a selected index. Reverse removal
        // preserves all other target heads and the relative live-event FIFO.
        for index in deferred.into_iter().rev() {
            self.state.remove(index);
        }
        chosen
    }
    pub fn take_http(&mut self, active: &HashMap<i64, Entry>) -> Option<Entry> {
        let index = self
            .http
            .iter()
            .position(|e| !active.contains_key(&e.channel))?;
        self.http.remove(index)
    }
}

#[cfg(test)]
mod tests;
