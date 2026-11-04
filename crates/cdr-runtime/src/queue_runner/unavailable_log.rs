use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use super::retry::retry_delay_for_error;

const REPEAT_REPORT_INTERVAL: Duration = Duration::from_mins(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UnavailableTargetReport {
    pub(crate) error: String,
    pub(crate) suppressed: u64,
}

#[derive(Debug, Default)]
pub(crate) struct UnavailableTargetLogState {
    targets: HashMap<String, TargetFailure>,
}

#[derive(Debug)]
struct TargetFailure {
    error: String,
    suppressed: u64,
    next_report_at: Instant,
    failure_count: i64,
    next_retry_at: Instant,
}

impl UnavailableTargetLogState {
    pub(crate) fn retry_at(&self, target: &str) -> Option<Instant> {
        self.targets
            .get(target)
            .map(|failure| failure.next_retry_at)
    }

    pub(crate) fn retry_due(&self, target: &str, now: Instant) -> bool {
        self.targets
            .get(target)
            .is_none_or(|failure| now >= failure.next_retry_at)
    }

    pub(crate) fn on_failure(
        &mut self,
        target: &str,
        error: &str,
        now: Instant,
    ) -> Option<UnavailableTargetReport> {
        let Some(failure) = self.targets.get_mut(target) else {
            self.targets
                .insert(target.to_owned(), TargetFailure::new(error, now));
            return Some(UnavailableTargetReport::immediate(error));
        };
        if failure.error != error {
            *failure = TargetFailure::new(error, now);
            return Some(UnavailableTargetReport::immediate(error));
        }
        failure.failure_count = failure.failure_count.saturating_add(1);
        failure.next_retry_at = next_retry_at(now, failure.failure_count, error);
        if now < failure.next_report_at {
            failure.suppressed = failure.suppressed.saturating_add(1);
            return None;
        }
        let report = UnavailableTargetReport {
            error: error.to_owned(),
            suppressed: failure.suppressed,
        };
        failure.suppressed = 0;
        failure.next_report_at = next_report_at(now);
        Some(report)
    }

    pub(crate) fn on_success(&mut self, target: &str) {
        self.targets.remove(target);
    }

    pub(crate) fn retain_targets(&mut self, active_targets: &BTreeSet<String>) {
        self.targets
            .retain(|target, _failure| active_targets.contains(target));
    }
}

impl TargetFailure {
    fn new(error: &str, now: Instant) -> Self {
        Self {
            error: error.to_owned(),
            suppressed: 0,
            next_report_at: next_report_at(now),
            failure_count: 1,
            next_retry_at: next_retry_at(now, 1, error),
        }
    }
}

impl UnavailableTargetReport {
    fn immediate(error: &str) -> Self {
        Self {
            error: error.to_owned(),
            suppressed: 0,
        }
    }
}

fn next_report_at(now: Instant) -> Instant {
    now.checked_add(REPEAT_REPORT_INTERVAL).unwrap_or(now)
}

fn next_retry_at(now: Instant, failure_count: i64, error: &str) -> Instant {
    now.checked_add(Duration::from_secs(u64::from(retry_delay_for_error(
        failure_count,
        error,
    ))))
    .unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};

    use super::{UnavailableTargetLogState, UnavailableTargetReport};

    const REPEAT_INTERVAL: Duration = Duration::from_mins(1);

    #[test]
    fn writer_gate_uses_five_ten_fifteen_and_changed_errors_reset_policy() {
        let error = "app-server returned error -32600 for thread/resume: thread x already has an active writer";
        let mut state = UnavailableTargetLogState::default();
        let mut now = Instant::now();
        for seconds in [5, 10, 15, 15] {
            state.on_failure("x", error, now);
            assert!(!state.retry_due("x", now + Duration::from_secs(seconds - 1)));
            now += Duration::from_secs(seconds);
            assert!(state.retry_due("x", now));
        }
        state.on_failure("x", "transport timeout", now);
        assert!(!state.retry_due("x", now + Duration::from_secs(15)));
        assert!(state.retry_due("x", now + Duration::from_secs(30)));
        state.on_failure("x", error, now);
        assert!(state.retry_due("x", now + Duration::from_secs(5)));
    }

    #[test]
    fn unavailable_target_retry_gate_uses_shared_exponential_schedule() {
        let started = Instant::now();
        let mut state = UnavailableTargetLogState::default();

        assert!(state.retry_due("thread-a", started));
        assert!(state.on_failure("thread-a", "offline", started).is_some());
        assert!(!state.retry_due("thread-a", started + Duration::from_secs(29)));
        assert!(state.retry_due("thread-a", started + Duration::from_secs(30)));

        let _ = state.on_failure("thread-a", "offline", started + Duration::from_secs(30));
        assert!(!state.retry_due("thread-a", started + Duration::from_secs(89)));
        assert!(state.retry_due("thread-a", started + Duration::from_secs(90)));
    }

    #[test]
    fn first_changed_and_periodic_failures_are_reported() {
        let started = Instant::now();
        let mut state = UnavailableTargetLogState::default();

        assert_eq!(
            state.on_failure("thread-a", "resume failed", started),
            Some(UnavailableTargetReport {
                error: "resume failed".into(),
                suppressed: 0,
            })
        );
        assert_eq!(
            state.on_failure(
                "thread-a",
                "resume failed",
                started + Duration::from_secs(59)
            ),
            None
        );
        assert_eq!(
            state.on_failure("thread-a", "resume failed", started + REPEAT_INTERVAL),
            Some(UnavailableTargetReport {
                error: "resume failed".into(),
                suppressed: 1,
            })
        );
        assert_eq!(
            state.on_failure("thread-a", "read failed", started + Duration::from_secs(61)),
            Some(UnavailableTargetReport {
                error: "read failed".into(),
                suppressed: 0,
            })
        );
    }

    #[test]
    fn success_resets_only_the_reconciled_target() {
        let started = Instant::now();
        let mut state = UnavailableTargetLogState::default();
        assert!(state.on_failure("thread-a", "offline", started).is_some());
        assert!(state.on_failure("thread-b", "offline", started).is_some());
        assert!(
            state
                .on_failure("thread-a", "offline", started + Duration::from_secs(1))
                .is_none()
        );
        state.on_success("thread-a");

        assert_eq!(
            state.on_failure("thread-a", "offline", started + Duration::from_secs(2)),
            Some(UnavailableTargetReport {
                error: "offline".into(),
                suppressed: 0,
            })
        );
        assert!(
            state
                .on_failure("thread-b", "offline", started + Duration::from_secs(2))
                .is_none()
        );
    }

    #[test]
    fn targets_no_longer_in_recovery_are_pruned() {
        let started = Instant::now();
        let mut state = UnavailableTargetLogState::default();
        assert!(state.on_failure("thread-a", "offline", started).is_some());
        assert!(state.on_failure("thread-b", "offline", started).is_some());

        state.retain_targets(&BTreeSet::from(["thread-b".to_owned()]));

        assert!(
            state
                .on_failure("thread-a", "offline", started + Duration::from_secs(1))
                .is_some()
        );
        assert!(
            state
                .on_failure("thread-b", "offline", started + Duration::from_secs(1))
                .is_none()
        );
    }
}
