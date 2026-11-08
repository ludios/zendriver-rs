//! Event expectation helpers (`expect_request` / `expect_response` /
//! `expect_dialog` / `expect_download` / `expect_file_chooser`).
//!
//! Each helper registers a one-shot subscription on a Tab's CDP event stream
//! and resolves with the first matching event. [`UrlMatcher`] is the shared
//! pattern type used by request/response expectations.
//!
//! ```no_run
//! # async fn ex() -> zendriver::Result<()> {
//! # let browser = zendriver::Browser::builder().launch().await?;
//! # let tab = browser.main_tab();
//! // Pre-register before triggering the action that causes the request.
//! let exp = tab.expect_response("/api/users");
//! tab.find().css("button#load").one().await?.click().await?;
//! let resp = exp.await?;
//! let body = resp.body().await?;
//! # let _ = body;
//! # Ok(()) }
//! ```

pub mod dialog;
pub mod download;
pub mod file_chooser;
pub mod request;
pub mod response;

pub use crate::url_matcher::UrlMatcher;

use futures::{Stream, StreamExt};
use zendriver_transport::SessionHandle;

use crate::error::{Result, ZendriverError};

/// Subscribe to `method`-shaped events scoped to `session`, decoded via
/// `T`'s [`serde::de::DeserializeOwned`] impl. Backs every `expect_*`
/// subscriber task, on [`SessionHandle::events`], which delivers the
/// session's `method` events in order and without gaps until it ends.
///
/// Yields `Ok(T)` for every successfully-decoded `method` event on this
/// session. Yields `Err(`[`ZendriverError::EventStreamIncomplete`]`)` on:
///
/// - The subscription's end, whatever its reason
///   ([`SessionEnd`](zendriver_transport::SessionEnd)): the session
///   detached (as when [`crate::Tab::close`] closes a tab without its own
///   socket), its socket died or was shut or replaced, or the waiting
///   expectation fell so far behind its events that their queue overflowed.
///   Either way the caller can no longer prove the awaited condition didn't
///   occur just before the end, so a "no show" (`Timeout`) would be a
///   possibly-wrong claim.
/// - A decode failure on an event that otherwise matches `method` +
///   `session` — the event we were waiting for arrived in a shape we can't
///   parse, which leaves us just as unable to confirm the awaited condition
///   as an outright loss would.
pub(crate) fn watch<T>(
    session: &SessionHandle,
    method: &'static str,
) -> impl Stream<Item = Result<T>> + Send + Unpin + use<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    Box::pin(session.events(&[method]).map(|next| {
        let event = next.map_err(|_| ZendriverError::EventStreamIncomplete)?;
        T::deserialize(&event.params).map_err(|_| ZendriverError::EventStreamIncomplete)
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::time::Duration;
    use tokio::time::timeout;
    use zendriver_transport::testing::MockConnection;

    // A flat session filters events by its `sessionId`: an event tagged with
    // the same id is delivered. This is the control for the root case below —
    // if it broke, the harness (not the model) would be at fault.
    #[tokio::test]
    async fn flat_session_watch_still_receives_its_events() {
        let (mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn, "S1");
        let mut stream = watch::<Value>(&sess, "Page.javascriptDialogOpening");
        mock.emit_event_for_session(
            "Page.javascriptDialogOpening",
            json!({ "message": "hi" }),
            "S1",
        )
        .await;
        let got = timeout(Duration::from_millis(500), stream.next()).await;
        assert!(
            matches!(got, Ok(Some(Ok(_)))),
            "flat session dropped its own event: {got:?}"
        );
    }

    // A root (per-tab socket) session carries no `sessionId`, and neither do
    // the events on its own socket. `session_id()` must therefore be `None`
    // here so the filter matches sessionless frames — a `""` sentinel would
    // make `None != Some("")` and drop every event the tab ever sees.
    #[tokio::test]
    async fn root_session_watch_receives_sessionless_events() {
        let (mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new_root(conn);
        let mut stream = watch::<Value>(&sess, "Page.javascriptDialogOpening");
        mock.emit_event("Page.javascriptDialogOpening", json!({ "message": "hi" }))
            .await;
        let got = timeout(Duration::from_millis(500), stream.next()).await;
        assert!(
            matches!(got, Ok(Some(Ok(_)))),
            "root session dropped its own event: {got:?}"
        );
    }
}
