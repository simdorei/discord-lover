use cdr_runtime::queue_runner::{pending_retry_due_at, pending_retry_is_due, retry_delay_seconds};

#[test]
fn durable_retry_delays_are_bounded_at_fifteen_minutes() {
    let cases = [
        (1, 30),
        (2, 60),
        (3, 120),
        (4, 240),
        (5, 480),
        (6, 900),
        (7, 900),
        (50, 900),
    ];
    for (failure_count, seconds) in cases {
        assert_eq!(retry_delay_seconds(failure_count), seconds);
        assert_eq!(
            pending_retry_due_at(failure_count, "transport failed", 100.0),
            Some(100.0 + f64::from(seconds))
        );
    }
    assert_eq!(pending_retry_due_at(0, "", 100.0), None);
}

#[test]
fn retry_is_due_only_at_the_boundary_and_clock_skew_does_not_bypass_it() {
    assert!(!pending_retry_is_due(1, "failed", 100.0, 90.0));
    assert!(!pending_retry_is_due(1, "failed", 100.0, 129.999));
    assert!(pending_retry_is_due(1, "failed", 100.0, 130.0));
    assert!(pending_retry_is_due(1, "failed", 100.0, 131.0));
    assert!(pending_retry_is_due(0, "", 100.0, 90.0));
}

#[test]
fn only_definite_resume_writer_rejections_get_short_backoff() {
    use cdr_runtime::queue_runner::retry_delay_for_error;
    let writer = "app-server returned error -32600 for thread/resume: thread writer-fast already has an active writer";
    for (count, delay) in [(1, 5), (2, 10), (3, 15), (9, 15), (i64::MAX, 15)] {
        assert_eq!(retry_delay_for_error(count, writer), delay);
        assert_eq!(
            pending_retry_due_at(count, writer, 100.0),
            Some(100.0 + f64::from(delay))
        );
    }
    for unsafe_error in [
        "already has an active writer",
        "thread/resume timed out",
        "app-server returned error -32600 for turn/start: thread writer-fast already has an active writer",
        "app-server returned error -32603 for thread/resume: thread writer-fast already has an active writer",
        "app-server returned error -32600 for thread/resume: thread  already has an active writer",
    ] {
        assert_eq!(retry_delay_for_error(9, unsafe_error), 900);
    }
}
