//! Whether one frame is loading, followed from Chrome's
//! `Page.frameStartedLoading` / `Page.frameStoppedLoading` — internal.
//!
//! Chrome (checked on 156) sends that pair for every kind of navigation of
//! a frame: cross-document, same-document (fragment, `pushState`),
//! back/forward (including a back-forward-cache restore), reload, a
//! download, and an error page. A navigation that starts while another is
//! loading sends a second start and no stop in between. So the frame is
//! loading exactly when a start has arrived since the last stop.
//!
//! The events come from a broadcast channel, which never replays an event
//! sent before subscribing, so a tracker subscribes when it is created:
//! create it before the action that navigates.
//!
//! When events are lost, the page's `document.readyState` says whether the
//! frame is loading, but only once the navigation has replaced the old
//! document, which reads as complete. So a navigation's tracker first asks
//! Chrome whether the frame shows the navigation's [`Destination`].

use std::time::Duration;

use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::warn;
use zendriver_transport::{AccountedRawEvent, SessionHandle};

use crate::error::{Result, ZendriverError};

/// What happened to the followed frame, as far as its events tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Started,
    Stopped,
    /// Events may have been missed: the subscriber lagged, or the
    /// connection was replaced.
    Lost,
}

/// Where a navigation of the main frame leads, as far as Chrome tells when
/// the command that starts it replies.
#[derive(Clone, Debug)]
pub(crate) enum Destination {
    /// The document with this loader id, which `Page.navigate` returned.
    Document(String),
    /// The current document: `Page.navigate` returned no loader id, because
    /// the navigation stays in the same document.
    SameDocument,
    /// Any document other than the one with this loader id: a reload's new
    /// document, whose loader id Chrome doesn't return.
    NotDocument(String),
    /// The session history entry with this id.
    HistoryEntry(i64),
}

impl Destination {
    /// Where the navigation that `Page.navigate` replied `reply` to leads.
    pub(crate) fn of_navigate_reply(reply: &Value) -> Self {
        match reply.get("loaderId").and_then(Value::as_str) {
            Some(id) => Self::Document(id.to_owned()),
            None => Self::SameDocument,
        }
    }
}

/// The loader id of the document in `session`'s main frame.
///
/// # Errors
///
/// The call's own error, or [`ZendriverError::Navigation`] when the reply
/// has no main-frame `loaderId`.
pub(crate) async fn main_frame_loader_id(session: &SessionHandle) -> Result<String> {
    let reply = session.call("Page.getFrameTree", json!({})).await?;
    let id = reply["frameTree"]["frame"]["loaderId"].as_str();
    id.map(str::to_owned).ok_or_else(|| {
        ZendriverError::Navigation("Page.getFrameTree returned no main-frame loaderId".into())
    })
}

/// The id of `session`'s current session history entry.
///
/// # Errors
///
/// The call's own error, or [`ZendriverError::HistoryNavigation`] when the
/// reply has no current entry id.
async fn current_history_entry(session: &SessionHandle) -> Result<i64> {
    let history = session.call("Page.getNavigationHistory", json!({})).await?;
    let id = history["currentIndex"]
        .as_u64()
        .and_then(|i| history["entries"].get(usize::try_from(i).ok()?))
        .and_then(|entry| entry["id"].as_i64());
    id.ok_or_else(|| {
        ZendriverError::HistoryNavigation(
            "Page.getNavigationHistory returned no current entry id".into(),
        )
    })
}

/// Whether `session`'s main frame shows the navigation's destination now.
/// Waits for `destination` to be known (until the command that starts the
/// navigation has replied). A failed call counts as no, and is logged.
async fn arrived(
    session: &SessionHandle,
    destination: &mut watch::Receiver<Option<Destination>>,
) -> bool {
    let destination = destination
        .wait_for(Option::is_some)
        .await
        .ok()
        .and_then(|known| known.clone());
    let Some(destination) = destination else {
        // The tracker is gone.
        return false;
    };
    let shown = match &destination {
        Destination::SameDocument => Ok(true),
        Destination::Document(id) => main_frame_loader_id(session).await.map(|now| now == *id),
        Destination::NotDocument(id) => main_frame_loader_id(session).await.map(|now| now != *id),
        Destination::HistoryEntry(id) => current_history_entry(session).await.map(|now| now == *id),
    };
    shown.unwrap_or_else(|e| {
        warn!(
            error = %e,
            ?destination,
            "can't tell whether the navigation replaced the old document; waiting for its stop"
        );
        false
    })
}

/// How a [`LoadTracker`] learns the frame's state before its first event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Initial {
    /// The caller is about to start a navigation: loading until it starts
    /// and then stops. A stop before the start belongs to an earlier load.
    Navigating,
    /// Evaluate this expression in the frame's main world; the frame has
    /// loaded iff it is `"complete"`.
    Probe(&'static str),
}

/// The changes of frame `frame_id` that `session` receives from now on, in
/// the order Chrome sent them. Ends when the socket dies, but not on a
/// clean [`zendriver_transport::Connection::shutdown`], which the event bus
/// can't tell from a reconnect.
fn changes(
    session: &SessionHandle,
    frame_id: &str,
) -> impl Stream<Item = Change> + Send + Unpin + use<> {
    let session_id = session.session_id().map(str::to_owned);
    let frame_id = frame_id.to_owned();
    // One subscription for both methods, so a start and a stop can't swap
    // places on the way here.
    Box::pin(
        session
            .connection()
            .subscribe_raw_accounted()
            .take_while(|ev| {
                let alive = !matches!(ev, AccountedRawEvent::Disconnected { .. });
                async move { alive }
            })
            .filter_map(move |ev| {
                let change = match ev {
                    AccountedRawEvent::Event { event, .. } => {
                        let ours = event.session_id == session_id
                            && event.params.get("frameId").and_then(|v| v.as_str())
                                == Some(&frame_id);
                        match event.method.as_str() {
                            "Page.frameStartedLoading" if ours => Some(Change::Started),
                            "Page.frameStoppedLoading" if ours => Some(Change::Stopped),
                            _ => None,
                        }
                    }
                    AccountedRawEvent::Lagged { .. } | AccountedRawEvent::Reconnected { .. } => {
                        Some(Change::Lost)
                    }
                    AccountedRawEvent::Disconnected { .. } => None,
                };
                async move { change }
            }),
    )
}

/// Whether `expression` evaluates to `"complete"` in `session`'s main
/// world, after enabling the Page domain (so every event after this call is
/// delivered). A failed evaluation counts as not complete, and is logged.
async fn probe_complete(session: &SessionHandle, expression: &str) -> bool {
    let reply = async {
        session.call("Page.enable", json!({})).await?;
        session
            .call(
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": true }),
            )
            .await
    }
    .await;
    match reply {
        Ok(reply) => reply["result"]["value"] == "complete",
        Err(e) => {
            warn!(
                error = %e,
                %expression,
                "load state probe failed; counting the frame as loading until Chrome says it stopped"
            );
            false
        }
    }
}

/// Feed `changes` into `loading`, starting from `initial`. `destination` is
/// where an `Initial::Navigating` navigation leads, once known.
async fn follow(
    mut changes: impl Stream<Item = Change> + Unpin,
    session: SessionHandle,
    initial: Initial,
    mut destination: watch::Receiver<Option<Destination>>,
    loading: watch::Sender<bool>,
) {
    let mut awaiting_start = false;
    // What to ask the page after lost events. `None` while a navigation
    // hasn't stopped and may not have replaced the old document, which
    // reads as complete.
    let mut reprobe = None;
    match initial {
        Initial::Navigating => awaiting_start = true,
        Initial::Probe(expression) => {
            reprobe = Some(expression);
            loading.send_replace(!probe_complete(&session, expression).await);
        }
    }
    while let Some(change) = changes.next().await {
        match change {
            Change::Started => {
                awaiting_start = false;
                loading.send_replace(true);
            }
            Change::Stopped => {
                if !awaiting_start {
                    loading.send_replace(false);
                    reprobe.get_or_insert("document.readyState");
                }
            }
            Change::Lost => {
                // Unknown until the page answers, or the next stop arrives.
                loading.send_replace(true);
                if reprobe.is_none() {
                    // The navigation's start, or even its stop, may be
                    // among the lost events: the next stop counts.
                    awaiting_start = false;
                    if arrived(&session, &mut destination).await {
                        reprobe = Some("document.readyState");
                    }
                }
                if let Some(expression) = reprobe {
                    loading.send_replace(!probe_complete(&session, expression).await);
                }
            }
        }
    }
}

/// Wait for the next `Page.frameStoppedLoading` of frame `frame_id` on
/// `session`, whatever started that load.
///
/// # Errors
///
/// [`ZendriverError::Timeout`] if none arrives within `within`;
/// [`ZendriverError::Navigation`] if the socket dies first.
pub(crate) async fn next_stop(
    session: &SessionHandle,
    frame_id: &str,
    within: Duration,
) -> Result<()> {
    let mut changes = changes(session, frame_id);
    let stop = async {
        while let Some(change) = changes.next().await {
            if change == Change::Stopped {
                return Some(());
            }
        }
        None
    };
    match tokio::time::timeout(within, stop).await {
        Err(_) => Err(ZendriverError::Timeout(within)),
        Ok(None) => Err(ZendriverError::Navigation(
            "the page's connection closed while waiting for the frame to load".into(),
        )),
        Ok(Some(_)) => Ok(()),
    }
}

/// Follows whether one frame is loading, in a task that runs until the
/// tracker is dropped or the socket dies.
#[derive(Debug)]
pub(crate) struct LoadTracker {
    loading: watch::Receiver<bool>,
    /// Where the navigation leads, once known; only a tracker made by
    /// [`LoadTracker::before_navigating`] reads it.
    destination: watch::Sender<Option<Destination>>,
    task: JoinHandle<()>,
}

impl LoadTracker {
    /// Follow frame `frame_id` on `session`, starting from `initial`.
    fn start(session: &SessionHandle, frame_id: &str, initial: Initial) -> Self {
        let changes = changes(session, frame_id);
        let (loading_tx, loading) = watch::channel(true);
        let (destination, destination_rx) = watch::channel(None);
        let task = tokio::spawn(follow(
            changes,
            session.clone(),
            initial,
            destination_rx,
            loading_tx,
        ));
        Self {
            loading,
            destination,
            task,
        }
    }

    /// For a caller that is about to start a navigation of the main frame,
    /// whose id is `frame_id`, on `session`: loading until that navigation
    /// has started and stopped. Call [`LoadTracker::heading_to`] once the
    /// command that starts it has replied.
    pub(crate) fn before_navigating(session: &SessionHandle, frame_id: &str) -> Self {
        Self::start(session, frame_id, Initial::Navigating)
    }

    /// Tell the tracker where its navigation leads. Until then, lost events
    /// leave it waiting for this.
    pub(crate) fn heading_to(&self, destination: Destination) {
        self.destination.send_replace(Some(destination));
    }

    /// For a frame whose state is unknown: loading until `expression`,
    /// evaluated in `session`'s main world in the tracker's task, says
    /// `"complete"` or a later stop arrives. The caller doesn't wait for
    /// the evaluation.
    pub(crate) fn probed(
        session: &SessionHandle,
        frame_id: &str,
        expression: &'static str,
    ) -> Self {
        Self::start(session, frame_id, Initial::Probe(expression))
    }

    /// Wait until the frame isn't loading.
    ///
    /// # Errors
    ///
    /// [`ZendriverError::Timeout`] if it is still loading after `within`;
    /// [`ZendriverError::Navigation`] if the socket dies first.
    pub(crate) async fn wait(mut self, within: Duration) -> Result<()> {
        match tokio::time::timeout(within, self.loading.wait_for(|loading| !loading)).await {
            Err(_) => Err(ZendriverError::Timeout(within)),
            Ok(Err(_)) => Err(ZendriverError::Navigation(
                "the page's connection closed while waiting for it to load".into(),
            )),
            Ok(Ok(_)) => Ok(()),
        }
    }
}

impl Drop for LoadTracker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use zendriver_transport::testing::MockConnection;

    const SHORT: Duration = Duration::from_millis(200);
    const LONG: Duration = Duration::from_secs(5);

    /// Emit `Page.frame{Started,Stopped}Loading` for `frame` on `session`.
    async fn emit(mock: &MockConnection, started: bool, frame: &str, session: &str) {
        let method = if started {
            "Page.frameStartedLoading"
        } else {
            "Page.frameStoppedLoading"
        };
        mock.emit_event_for_session(method, json!({ "frameId": frame }), session)
            .await;
    }

    /// A stop that arrives before the navigation's start belongs to an
    /// earlier load, and doesn't end the wait.
    #[tokio::test]
    async fn before_navigating_ignores_a_stop_before_the_start() {
        let (mock, conn) = MockConnection::pair();
        let tracker = LoadTracker::before_navigating(&SessionHandle::new(conn.clone(), "S1"), "F");
        emit(&mock, false, "F", "S1").await;
        let res = tracker.wait(SHORT).await;
        assert!(matches!(res, Err(ZendriverError::Timeout(_))), "{res:?}");
        conn.shutdown();
    }

    /// Only the followed frame on the followed session counts.
    #[tokio::test]
    async fn before_navigating_ends_at_the_frames_own_stop() {
        let (mock, conn) = MockConnection::pair();
        let session = SessionHandle::new(conn.clone(), "S1");
        let tracker = LoadTracker::before_navigating(&session, "F");
        emit(&mock, true, "F", "S1").await;
        emit(&mock, false, "G", "S1").await;
        emit(&mock, false, "F", "S2").await;
        let res = tracker.wait(SHORT).await;
        assert!(matches!(res, Err(ZendriverError::Timeout(_))), "{res:?}");

        let tracker = LoadTracker::before_navigating(&session, "F");
        emit(&mock, true, "F", "S1").await;
        emit(&mock, false, "F", "S1").await;
        tracker.wait(LONG).await.expect("the frame's own stop");
        conn.shutdown();
    }

    /// A probed tracker enables Page, evaluates the expression, and starts
    /// from what it says: `"complete"` is loaded, anything else loading
    /// until a stop arrives.
    #[tokio::test]
    async fn probed_starts_from_the_probe() {
        for (value, loaded) in [("complete", true), ("blank", false)] {
            let (mut mock, conn) = MockConnection::pair();
            let tracker = LoadTracker::probed(&SessionHandle::new(conn.clone(), "S1"), "F", "x");
            let id = mock.expect_cmd("Page.enable").await;
            mock.reply(id, json!({})).await;
            let id = mock.expect_cmd("Runtime.evaluate").await;
            assert_eq!(mock.last_sent()["params"]["expression"], "x");
            mock.reply(id, json!({ "result": { "value": Value::from(value) } }))
                .await;
            if loaded {
                tracker.wait(LONG).await.expect("already loaded");
            } else {
                let waiting = tokio::spawn(tracker.wait(LONG));
                emit(&mock, false, "F", "S1").await;
                waiting.await.unwrap().expect("loaded once it stopped");
            }
            conn.shutdown();
        }
    }

    /// Wait for the tracker to send `method`, and reply `reply`.
    async fn answer(mock: &mut MockConnection, method: &str, reply: Value) {
        let id = tokio::time::timeout(LONG, mock.expect_cmd(method))
            .await
            .unwrap_or_else(|_| panic!("the tracker didn't send {method}"));
        mock.reply(id, reply).await;
    }

    /// After lost events, a navigation's tracker asks Chrome whether the
    /// frame shows the navigation's destination. Until it does, the page is
    /// the old document, still complete, so the tracker doesn't ask the
    /// page: it waits for the next stop instead.
    #[tokio::test]
    async fn lost_events_leave_a_navigation_waiting_until_it_arrives() {
        let (mut mock, conn) = MockConnection::pair();
        let tracker = LoadTracker::before_navigating(&SessionHandle::new(conn.clone(), "S1"), "F");
        tracker.heading_to(Destination::Document("new".into()));
        mock.reconnect(&conn);
        let waiting = tokio::spawn(tracker.wait(LONG));
        let tree = json!({ "frameTree": { "frame": { "id": "F", "loaderId": "old" } } });
        answer(&mut mock, "Page.getFrameTree", tree).await;
        let probe = tokio::time::timeout(SHORT, mock.expect_cmd("Page.enable")).await;
        assert!(
            probe.is_err(),
            "the tracker probed the page after lost events"
        );
        emit(&mock, false, "F", "S1").await;
        waiting.await.unwrap().expect("loaded once it stopped");
        conn.shutdown();
    }

    /// After lost events, a navigation's tracker that finds the frame at the
    /// navigation's destination trusts the page's `readyState`, so a lost
    /// stop doesn't leave it waiting.
    #[tokio::test]
    async fn lost_events_after_a_navigation_arrives_make_it_probe() {
        let tree = |loader: &str| json!({ "frameTree": { "frame": { "loaderId": loader } } });
        let history = json!({ "currentIndex": 1, "entries": [{ "id": 7 }, { "id": 8 }] });
        let cases = [
            (
                Destination::Document("new".into()),
                Some(("Page.getFrameTree", tree("new"))),
            ),
            (
                Destination::NotDocument("old".into()),
                Some(("Page.getFrameTree", tree("new"))),
            ),
            (
                Destination::HistoryEntry(8),
                Some(("Page.getNavigationHistory", history)),
            ),
            (Destination::SameDocument, None),
        ];
        for (destination, question) in cases {
            let (mut mock, conn) = MockConnection::pair();
            let tracker =
                LoadTracker::before_navigating(&SessionHandle::new(conn.clone(), "S1"), "F");
            mock.reconnect(&conn);
            // The tracker waits for the destination before it asks.
            let early = mock.recv_cmd_timeout(SHORT).await;
            assert_eq!(early, None, "asked before the destination was known");
            tracker.heading_to(destination.clone());
            if let Some((method, reply)) = question {
                answer(&mut mock, method, reply).await;
            }
            answer(&mut mock, "Page.enable", json!({})).await;
            let complete = json!({ "result": { "value": "complete" } });
            answer(&mut mock, "Runtime.evaluate", complete).await;
            assert_eq!(
                mock.last_sent()["params"]["expression"],
                "document.readyState"
            );
            tracker
                .wait(LONG)
                .await
                .unwrap_or_else(|e| panic!("{destination:?}: {e}"));
            conn.shutdown();
        }
    }

    /// After lost events, a navigation's tracker that has seen its stop
    /// asks the page again: a later navigation's start may have been lost.
    #[tokio::test]
    async fn lost_events_after_the_stop_make_a_navigation_probe() {
        let (mut mock, conn) = MockConnection::pair();
        let tracker = LoadTracker::before_navigating(&SessionHandle::new(conn.clone(), "S1"), "F");
        let mut seen = changes(&SessionHandle::new(conn.clone(), "S1"), "F");
        emit(&mock, true, "F", "S1").await;
        emit(&mock, false, "F", "S1").await;
        // A reconnect drops frames the old socket hasn't delivered yet.
        assert_eq!(seen.next().await, Some(Change::Started));
        assert_eq!(seen.next().await, Some(Change::Stopped));
        mock.reconnect(&conn);
        let id = tokio::time::timeout(LONG, mock.expect_cmd("Page.enable"))
            .await
            .expect("no probe after lost events");
        mock.reply(id, json!({})).await;
        let id = mock.expect_cmd("Runtime.evaluate").await;
        assert_eq!(
            mock.last_sent()["params"]["expression"],
            "document.readyState"
        );
        mock.reply(id, json!({ "result": { "value": "loading" } }))
            .await;
        let res = tracker.wait(SHORT).await;
        assert!(matches!(res, Err(ZendriverError::Timeout(_))), "{res:?}");
        conn.shutdown();
    }

    /// After lost events, a probed tracker probes again with its own
    /// expression.
    #[tokio::test]
    async fn lost_events_make_a_probed_tracker_probe_again() {
        let (mut mock, conn) = MockConnection::pair();
        let tracker = LoadTracker::probed(&SessionHandle::new(conn.clone(), "S1"), "F", "x");
        let id = mock.expect_cmd("Page.enable").await;
        mock.reply(id, json!({})).await;
        let id = mock.expect_cmd("Runtime.evaluate").await;
        mock.reply(id, json!({ "result": { "value": "blank" } }))
            .await;
        let waiting = tokio::spawn(tracker.wait(LONG));
        mock.reconnect(&conn);
        let id = mock.expect_cmd("Page.enable").await;
        mock.reply(id, json!({})).await;
        let id = mock.expect_cmd("Runtime.evaluate").await;
        assert_eq!(mock.last_sent()["params"]["expression"], "x");
        mock.reply(id, json!({ "result": { "value": "complete" } }))
            .await;
        waiting
            .await
            .unwrap()
            .expect("complete on the second probe");
        conn.shutdown();
    }

    /// When the socket dies, the wait ends at once with an error rather than
    /// at its timeout.
    #[tokio::test]
    async fn wait_ends_when_the_connection_dies() {
        let (mock, conn) = MockConnection::pair();
        let tracker = LoadTracker::before_navigating(&SessionHandle::new(conn.clone(), "S1"), "F");
        mock.disconnect();
        let res = tracker.wait(LONG).await;
        assert!(matches!(res, Err(ZendriverError::Navigation(_))), "{res:?}");
    }
}
