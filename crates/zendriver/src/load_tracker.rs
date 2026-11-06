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
//! After lost events, the page's `document.readyState` says whether the
//! frame is loading, but only after the load in progress has replaced the old
//! document, which reads as complete. So a tracker whose own navigation
//! hasn't stopped first asks Chrome whether the frame shows the
//! navigation's [`Destination`], and a tracker that saw another load start
//! waits for that load's stop.

use std::pin::Pin;
use std::time::Duration;

use futures::stream::Peekable;
use futures::{FutureExt, Stream, StreamExt};
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
    /// Not known: `Page.reload` and `Page.navigateToHistoryEntry` return
    /// nothing. (Asking the renderer for the loader id before the reload
    /// would need a working renderer to reload a crashed tab, and the
    /// browser reports a pending history entry as the current one.)
    Unknown,
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
async fn main_frame_loader_id(session: &SessionHandle) -> Result<String> {
    let reply = session.call("Page.getFrameTree", json!({})).await?;
    let id = reply["frameTree"]["frame"]["loaderId"].as_str();
    id.map(str::to_owned).ok_or_else(|| {
        ZendriverError::Navigation("Page.getFrameTree returned no main-frame loaderId".into())
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
        Destination::Unknown => Ok(false),
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

/// The load a [`LoadTracker`] waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Initial {
    /// A navigation the caller is about to start: loading until it starts
    /// and then stops. A stop before the start belongs to an earlier load.
    Navigating,
    /// A new tab's first navigation, which may not have started: loading
    /// until the page has left `about:blank` and loaded, or a stop arrives.
    NewTab,
    /// The current document's: loading until it has loaded, or a stop
    /// arrives.
    Current,
}

/// The changes of frame `frame_id` that `session` receives from now on, in
/// the order Chrome sent them. Ends when the socket dies or the connection
/// is shut down.
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
                let alive = !matches!(
                    ev,
                    AccountedRawEvent::Disconnected { .. } | AccountedRawEvent::Shutdown { .. }
                );
                async move { alive }
            })
            .filter_map(move |ev| {
                let change =
                    match ev {
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
                        AccountedRawEvent::Lagged { .. }
                        | AccountedRawEvent::Reconnected { .. } => Some(Change::Lost),
                        AccountedRawEvent::Disconnected { .. }
                        | AccountedRawEvent::Shutdown { .. } => None,
                    };
                async move { change }
            }),
    )
}

/// What a tracker does after lost events to learn whether the frame is
/// loading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recheck {
    /// The tracker's own navigation hasn't stopped, and no other load has
    /// started: if the frame shows the navigation's destination, ask the
    /// page's `document.readyState`; otherwise wait for the next stop.
    Arrival,
    /// A new tab's first navigation hasn't stopped, and no other load has
    /// started: ask the page [`LEFT_BLANK`].
    NewTab,
    /// No load has started since the frame last stopped, or since the
    /// tracker began: ask the page [`READY_STATE`].
    ReadyState,
    /// Wait for the next stop. A load the tracker didn't start has started,
    /// and until its document commits, the page is the old document, which
    /// reads as complete.
    NextStop,
}

/// Whether the current document has loaded: `"complete"` if so.
const READY_STATE: &str = "document.readyState";

/// Whether a new tab has left `about:blank` and loaded: `"complete"` if so.
const LEFT_BLANK: &str = "location.href === 'about:blank' ? 'blank' : document.readyState";

impl Recheck {
    /// What to ask the page, if the page can tell whether the load has
    /// finished.
    fn expression(self) -> Option<&'static str> {
        match self {
            Self::NewTab => Some(LEFT_BLANK),
            Self::ReadyState => Some(READY_STATE),
            Self::Arrival | Self::NextStop => None,
        }
    }
}

/// Whether `expression` evaluates to `"complete"` in `session`'s main
/// world, after enabling the Page domain (so every event after this call is
/// delivered). `None` if the evaluation failed, which is logged.
async fn probe_complete(session: &SessionHandle, expression: &str) -> Option<bool> {
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
        Ok(reply) => Some(reply["result"]["value"] == "complete"),
        Err(e) => {
            warn!(
                error = %e,
                %expression,
                "load state probe failed; leaving the load state as it was"
            );
            None
        }
    }
}

/// Whether a change is already waiting in `changes`, without waiting for
/// one.
fn change_queued(changes: &mut Peekable<impl Stream<Item = Change> + Unpin>) -> bool {
    Pin::new(changes).peek().now_or_never().flatten().is_some()
}

/// Feed `changes` into `loading`, for the load `initial`. `destination` is
/// where an `Initial::Navigating` navigation leads, after its command has
/// replied.
async fn follow(
    changes: impl Stream<Item = Change> + Unpin,
    session: SessionHandle,
    initial: Initial,
    mut destination: watch::Receiver<Option<Destination>>,
    loading: watch::Sender<bool>,
) {
    let mut changes = changes.peekable();
    let mut recheck = match initial {
        Initial::Navigating => Recheck::Arrival,
        Initial::NewTab => Recheck::NewTab,
        Initial::Current => Recheck::ReadyState,
    };
    // Whether a load has started since the tracker began, or may have
    // among lost events.
    let mut started = false;
    // Take the page's answer `complete` to `recheck`'s expression, given
    // whether the frame was loading before the question (a failed question
    // leaves that). A complete page counts as a stop. A change that arrived
    // while the page answered is newer than the answer, so the frame stays
    // loading until that change is taken.
    let answer = |complete: Option<bool>,
                  was_loading: bool,
                  recheck: &mut Recheck,
                  changes: &mut Peekable<_>| {
        if complete == Some(true) {
            *recheck = Recheck::ReadyState;
        }
        if !change_queued(changes) {
            loading.send_replace(complete.map_or(was_loading, |complete| !complete));
        }
    };
    if let Some(expression) = recheck.expression() {
        let complete = probe_complete(&session, expression).await;
        answer(complete, true, &mut recheck, &mut changes);
    }
    while let Some(change) = changes.next().await {
        match change {
            Change::Started => {
                // After the frame stopped, or a second time: another load.
                if started || recheck == Recheck::ReadyState {
                    recheck = Recheck::NextStop;
                }
                started = true;
                loading.send_replace(true);
            }
            Change::Stopped => {
                // A stop before the navigation's start ends an earlier load.
                if started || recheck != Recheck::Arrival {
                    loading.send_replace(false);
                    recheck = Recheck::ReadyState;
                }
            }
            Change::Lost => {
                // Unknown until the page answers, or the next stop arrives.
                let was_loading = loading.send_replace(true);
                started = true;
                if recheck == Recheck::Arrival && arrived(&session, &mut destination).await {
                    recheck = Recheck::ReadyState;
                }
                if let Some(expression) = recheck.expression() {
                    let complete = probe_complete(&session, expression).await;
                    answer(complete, was_loading, &mut recheck, &mut changes);
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
/// [`ZendriverError::Navigation`] if the connection ends first (the socket
/// dies, or the connection is shut down).
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
/// tracker is dropped or the connection ends (the socket dies, or the
/// connection is shut down).
#[derive(Debug)]
pub(crate) struct LoadTracker {
    loading: watch::Receiver<bool>,
    /// Where the navigation leads, after its command has replied; only a
    /// tracker made by [`LoadTracker::before_navigating`] reads it.
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
    /// has started and stopped. Call [`LoadTracker::heading_to`] after the
    /// command that starts it has replied.
    pub(crate) fn before_navigating(session: &SessionHandle, frame_id: &str) -> Self {
        Self::start(session, frame_id, Initial::Navigating)
    }

    /// Tell the tracker where its navigation leads. Until then, lost events
    /// leave it waiting for this.
    pub(crate) fn heading_to(&self, destination: Destination) {
        self.destination.send_replace(Some(destination));
    }

    /// For a new tab, whose main frame's id is `frame_id`, on `session`:
    /// loading until its first navigation, which may not have started, has
    /// left `about:blank` and loaded. Asks the page in the tracker's task;
    /// the caller doesn't wait for the answer.
    pub(crate) fn for_new_tab(session: &SessionHandle, frame_id: &str) -> Self {
        Self::start(session, frame_id, Initial::NewTab)
    }

    /// For the current document of frame `frame_id` on `session`: loading
    /// until it has loaded. Asks the page in the tracker's task; the caller
    /// doesn't wait for the answer.
    pub(crate) fn current(session: &SessionHandle, frame_id: &str) -> Self {
        Self::start(session, frame_id, Initial::Current)
    }

    /// Wait until the frame isn't loading.
    ///
    /// # Errors
    ///
    /// [`ZendriverError::Timeout`] if it is still loading after `within`;
    /// [`ZendriverError::Navigation`] if the connection ends first (the
    /// socket dies, or the connection is shut down).
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
    use zendriver_transport::Connection;
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

    /// Emit frame `F`'s starts (`true`) and stops (`false`) on session `S1`,
    /// and return after they are on the event bus, so a tracker gets them
    /// before anything that happens later. (A reconnect drops the frames the
    /// old socket hasn't delivered yet.)
    async fn emit_delivered(mock: &MockConnection, conn: &Connection, started: &[bool]) {
        let mut seen = changes(&SessionHandle::new(conn.clone(), "S1"), "F");
        for &s in started {
            emit(mock, s, "F", "S1").await;
        }
        for _ in started {
            seen.next().await;
        }
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

    /// The tracker for the current document, or a new tab's, enables Page,
    /// asks the page, and starts from what it says: `"complete"` is loaded,
    /// anything else loading until a stop arrives.
    #[tokio::test]
    async fn current_and_new_tab_start_from_the_page() {
        let cases = [
            (false, READY_STATE, "complete", true),
            (false, READY_STATE, "loading", false),
            (true, LEFT_BLANK, "complete", true),
            (true, LEFT_BLANK, "blank", false),
        ];
        for (new_tab, expression, value, loaded) in cases {
            let (mut mock, conn) = MockConnection::pair();
            let session = SessionHandle::new(conn.clone(), "S1");
            let tracker = if new_tab {
                LoadTracker::for_new_tab(&session, "F")
            } else {
                LoadTracker::current(&session, "F")
            };
            let id = mock.expect_cmd("Page.enable").await;
            mock.reply(id, json!({})).await;
            let id = mock.expect_cmd("Runtime.evaluate").await;
            assert_eq!(mock.last_sent()["params"]["expression"], expression);
            mock.reply(id, json!({ "result": { "value": Value::from(value) } }))
                .await;
            if loaded {
                tracker.wait(LONG).await.expect("already loaded");
            } else {
                let waiting = tokio::spawn(tracker.wait(LONG));
                emit(&mock, false, "F", "S1").await;
                waiting.await.unwrap().expect("loaded after it stopped");
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
        waiting.await.unwrap().expect("loaded after it stopped");
        conn.shutdown();
    }

    /// After lost events, a navigation's tracker that finds the frame at the
    /// navigation's destination trusts the page's `readyState`, so a lost
    /// stop doesn't leave it waiting.
    #[tokio::test]
    async fn lost_events_after_a_navigation_arrives_make_it_probe() {
        let tree = json!({ "frameTree": { "frame": { "loaderId": "new" } } });
        let cases = [
            (
                Destination::Document("new".into()),
                Some(("Page.getFrameTree", tree)),
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
        emit_delivered(&mock, &conn, &[true, false]).await;
        mock.reconnect(&conn);
        answer(&mut mock, "Page.enable", json!({})).await;
        let loading = json!({ "result": { "value": "loading" } });
        answer(&mut mock, "Runtime.evaluate", loading).await;
        assert_eq!(mock.last_sent()["params"]["expression"], READY_STATE);
        let res = tracker.wait(SHORT).await;
        assert!(matches!(res, Err(ZendriverError::Timeout(_))), "{res:?}");
        conn.shutdown();
    }

    /// A change that arrives while the page answers is newer than the
    /// answer: a later load's start that comes in while the tracker asks
    /// whether its navigation arrived keeps it loading, though the page then
    /// says complete. (Only the outcome is checked: the tracker must not
    /// publish "loaded" before it takes the start, but on this test's
    /// current-thread runtime, no waiter could run in between anyway.)
    #[tokio::test]
    async fn a_start_during_the_questions_beats_the_answer() {
        let (mut mock, conn) = MockConnection::pair();
        let tracker = LoadTracker::before_navigating(&SessionHandle::new(conn.clone(), "S1"), "F");
        tracker.heading_to(Destination::Document("new".into()));
        emit_delivered(&mock, &conn, &[true]).await;
        mock.reconnect(&conn);
        let id = tokio::time::timeout(LONG, mock.expect_cmd("Page.getFrameTree"))
            .await
            .expect("no arrival check after lost events");
        // Chrome sends the event before the reply.
        emit(&mock, true, "F", "S1").await;
        let tree = json!({ "frameTree": { "frame": { "loaderId": "new" } } });
        mock.reply(id, tree).await;
        answer(&mut mock, "Page.enable", json!({})).await;
        let complete = json!({ "result": { "value": "complete" } });
        answer(&mut mock, "Runtime.evaluate", complete).await;
        let res = tracker.wait(SHORT).await;
        assert!(matches!(res, Err(ZendriverError::Timeout(_))), "{res:?}");
        conn.shutdown();
    }

    /// When the probe after lost events fails, the tracker's state stays as
    /// it was: a navigation that has stopped stays loaded.
    #[tokio::test]
    async fn a_failed_probe_leaves_a_stopped_navigation_loaded() {
        let (mut mock, conn) = MockConnection::pair();
        let tracker = LoadTracker::before_navigating(&SessionHandle::new(conn.clone(), "S1"), "F");
        emit_delivered(&mock, &conn, &[true, false]).await;
        mock.reconnect(&conn);
        let id = tokio::time::timeout(LONG, mock.expect_cmd("Page.enable"))
            .await
            .expect("no probe after lost events");
        mock.reply_err(id, -32000, "No session with given id").await;
        tracker.wait(LONG).await.expect("still loaded");
        conn.shutdown();
    }

    /// After a load that the tracker didn't start has started, the page is
    /// the old document until the new one commits, so after lost events the
    /// tracker doesn't ask the page: it waits for the next stop. Such a load
    /// starts after the tracker's own load stopped (or was found loaded), or
    /// replaces it before its stop (a second start).
    #[tokio::test]
    async fn lost_events_during_a_later_load_wait_for_its_stop() {
        let cases = [
            (
                "navigation, start, stop, start",
                None,
                vec![true, false, true],
            ),
            ("navigation, start, start", None, vec![true, true]),
            ("current document, loaded, start", Some(false), vec![true]),
            ("new tab, loaded, start", Some(true), vec![true]),
            ("new tab, blank, start, start", Some(true), vec![true, true]),
        ];
        for (case, probed, starts_and_stops) in cases {
            let (mut mock, conn) = MockConnection::pair();
            let session = SessionHandle::new(conn.clone(), "S1");
            let tracker = match probed {
                None => {
                    let tracker = LoadTracker::before_navigating(&session, "F");
                    tracker.heading_to(Destination::Document("new".into()));
                    tracker
                }
                Some(new_tab) => {
                    let tracker = if new_tab {
                        LoadTracker::for_new_tab(&session, "F")
                    } else {
                        LoadTracker::current(&session, "F")
                    };
                    answer(&mut mock, "Page.enable", json!({})).await;
                    let value = if case.contains("blank") {
                        "blank"
                    } else {
                        "complete"
                    };
                    let reply = json!({ "result": { "value": value } });
                    answer(&mut mock, "Runtime.evaluate", reply).await;
                    tracker
                }
            };
            emit_delivered(&mock, &conn, &starts_and_stops).await;
            mock.reconnect(&conn);
            let waiting = tokio::spawn(tracker.wait(LONG));
            let probe = mock.recv_cmd_timeout(SHORT).await;
            assert_eq!(probe, None, "{case}");
            emit(&mock, false, "F", "S1").await;
            waiting.await.unwrap().expect("loaded after it stopped");
            conn.shutdown();
        }
    }

    /// After lost events, a new tab's tracker asks the page again whether
    /// it has left about:blank and loaded.
    #[tokio::test]
    async fn lost_events_make_a_new_tabs_tracker_ask_again() {
        let (mut mock, conn) = MockConnection::pair();
        let tracker = LoadTracker::for_new_tab(&SessionHandle::new(conn.clone(), "S1"), "F");
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
        assert_eq!(mock.last_sent()["params"]["expression"], LEFT_BLANK);
        mock.reply(id, json!({ "result": { "value": "complete" } }))
            .await;
        waiting
            .await
            .unwrap()
            .expect("complete on the second probe");
        conn.shutdown();
    }

    /// After its first navigation has stopped, a new tab's question (has the
    /// page left about:blank?) no longer applies: after lost events its
    /// tracker asks for the `readyState`.
    #[tokio::test]
    async fn a_stop_makes_a_new_tabs_tracker_ask_the_ready_state() {
        let (mut mock, conn) = MockConnection::pair();
        let tracker = LoadTracker::for_new_tab(&SessionHandle::new(conn.clone(), "S1"), "F");
        answer(&mut mock, "Page.enable", json!({})).await;
        let blank = json!({ "result": { "value": "blank" } });
        answer(&mut mock, "Runtime.evaluate", blank).await;
        emit_delivered(&mock, &conn, &[true, false]).await;
        mock.reconnect(&conn);
        answer(&mut mock, "Page.enable", json!({})).await;
        let complete = json!({ "result": { "value": "complete" } });
        answer(&mut mock, "Runtime.evaluate", complete).await;
        assert_eq!(mock.last_sent()["params"]["expression"], READY_STATE);
        tracker.wait(LONG).await.expect("loaded");
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

    /// And when the connection is shut down (as `Tab::close` does to a tab
    /// that has its own socket).
    #[tokio::test]
    async fn wait_ends_when_the_connection_is_shut_down() {
        let (_mock, conn) = MockConnection::pair();
        let tracker = LoadTracker::before_navigating(&SessionHandle::new(conn.clone(), "S1"), "F");
        conn.shutdown();
        let res = tokio::time::timeout(LONG, tracker.wait(LONG))
            .await
            .unwrap();
        assert!(matches!(res, Err(ZendriverError::Navigation(_))), "{res:?}");
    }
}
