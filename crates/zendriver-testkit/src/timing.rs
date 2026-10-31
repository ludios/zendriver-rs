//! Assertions about when things happen.

use std::future::Future;
use std::time::{Duration, Instant};

/// How often [`assert_eventually`] checks again.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Check `condition` until it's true; panic, naming `what`, if it isn't
/// within `within`. A check that hangs past the deadline also panics, so a
/// wedged CDP call can't outlast the bound.
///
/// `what` describes the condition, e.g. `"the iframe is attached"`.
pub async fn assert_eventually<F, Fut>(within: Duration, what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, condition()).await {
            Ok(true) => return,
            Ok(false) => {}
            Err(_) => panic!("still checking after {within:?}: {what}"),
        }
        // Stop rather than start a check the deadline would cut short.
        if tokio::time::Instant::now() + POLL_INTERVAL >= deadline {
            panic!("not true within {within:?}: {what}");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Assert that a wait which returned at `returned_at` didn't return before
/// `event_at`, the moment it should have waited for. `event` names it.
///
/// Take `returned_at` right after the wait, and `event_at` from something
/// that recorded when the event happened, such as
/// [`FixtureServer::last_responded_at`](crate::FixtureServer::last_responded_at).
pub fn assert_not_before(returned_at: Instant, event_at: Instant, event: &str) {
    assert!(
        returned_at >= event_at,
        "returned {:?} before {event}",
        event_at - returned_at
    );
}
