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
//! A tracker gets the events of its session that arrive after it is
//! created, all of them and in order ([`SessionHandle::events`]), so create
//! it before the action that navigates. A tracker for a load that may have
//! begun before it was created asks the page's `document.readyState`
//! first.

use std::pin::Pin;
use std::time::Duration;

use futures::stream::Peekable;
use futures::{FutureExt, Stream, StreamExt};
use serde_json::json;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::warn;
use zendriver_transport::SessionHandle;

use crate::error::{Result, ZendriverError};

/// What happened to the followed frame, as far as its events tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Started,
    Stopped,
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

impl Initial {
    /// What to ask the page first, for a load that may have begun before
    /// the tracker: `"complete"` means it has finished.
    fn question(self) -> Option<&'static str> {
        match self {
            Self::Navigating => None,
            Self::NewTab => Some(LEFT_BLANK),
            Self::Current => Some(READY_STATE),
        }
    }
}

/// The changes of frame `frame_id` that `session` receives from now on, in
/// the order Chrome sent them. Ends when the session's events end (the
/// socket dies or is replaced, the connection is shut down, or Chrome
/// detaches the session).
fn changes(
    session: &SessionHandle,
    frame_id: &str,
) -> impl Stream<Item = Change> + Send + Unpin + use<> {
    let frame_id = frame_id.to_owned();
    let events = session.events(&["Page.frameStartedLoading", "Page.frameStoppedLoading"]);
    Box::pin(
        events
            .take_while(|event| std::future::ready(event.is_ok()))
            .filter_map(move |event| {
                let change = event.ok().and_then(|event| {
                    if event.params["frameId"] != frame_id.as_str() {
                        return None;
                    }
                    match event.method.as_str() {
                        "Page.frameStartedLoading" => Some(Change::Started),
                        _ => Some(Change::Stopped),
                    }
                });
                std::future::ready(change)
            }),
    )
}

/// Whether the current document has loaded: `"complete"` if so.
const READY_STATE: &str = "document.readyState";

/// Whether a new tab has left `about:blank` and loaded: `"complete"` if so.
const LEFT_BLANK: &str = "location.href === 'about:blank' ? 'blank' : document.readyState";

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
                "load state probe failed; taking the frame as loading"
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

/// Feed `changes` into `loading`, for the load `initial`.
async fn follow(
    changes: impl Stream<Item = Change> + Unpin,
    session: SessionHandle,
    initial: Initial,
    loading: watch::Sender<bool>,
) {
    let mut changes = changes.peekable();
    if let Some(question) = initial.question() {
        let complete = probe_complete(&session, question).await;
        // A change that arrived while the page answered is newer than the
        // answer, so the frame stays loading until that change is taken.
        if !change_queued(&mut changes) {
            loading.send_replace(complete != Some(true));
        }
    }
    // Whether a stop ends the load the tracker waits for: for a navigation,
    // only once the navigation has started.
    let mut started = initial != Initial::Navigating;
    while let Some(change) = changes.next().await {
        match change {
            Change::Started => {
                started = true;
                loading.send_replace(true);
            }
            Change::Stopped => {
                if started {
                    loading.send_replace(false);
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
/// [`ZendriverError::Navigation`] if the changes end first (the socket
/// dies or is replaced, the connection is shut down, or Chrome detaches
/// the session).
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
            "the frame's session or connection ended while waiting for the frame to load".into(),
        )),
        Ok(Some(_)) => Ok(()),
    }
}

/// Follows whether one frame is loading, in a task that runs until the
/// tracker is dropped or the changes end (the socket dies or is replaced,
/// the connection is shut down, or Chrome detaches the session).
#[derive(Debug)]
pub(crate) struct LoadTracker {
    loading: watch::Receiver<bool>,
    task: JoinHandle<()>,
}

impl LoadTracker {
    /// Follow frame `frame_id` on `session`, starting from `initial`.
    fn start(session: &SessionHandle, frame_id: &str, initial: Initial) -> Self {
        let changes = changes(session, frame_id);
        let (loading_tx, loading) = watch::channel(true);
        let task = tokio::spawn(follow(changes, session.clone(), initial, loading_tx));
        Self { loading, task }
    }

    /// For a caller that is about to start a navigation of the main frame,
    /// whose id is `frame_id`, on `session`: loading until that navigation
    /// has started and stopped.
    pub(crate) fn before_navigating(session: &SessionHandle, frame_id: &str) -> Self {
        Self::start(session, frame_id, Initial::Navigating)
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
    /// [`ZendriverError::Navigation`] if the changes end first (the socket
    /// dies or is replaced, the connection is shut down, or Chrome detaches
    /// the session).
    pub(crate) async fn wait(mut self, within: Duration) -> Result<()> {
        match tokio::time::timeout(within, self.loading.wait_for(|loading| !loading)).await {
            Err(_) => Err(ZendriverError::Timeout(within)),
            Ok(Err(_)) => Err(ZendriverError::Navigation(
                "the frame's session or connection ended while waiting for it to load".into(),
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
    use serde_json::Value;
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

    /// A change that arrives while the page answers is newer than the
    /// answer: a start that comes in while the tracker for the current
    /// document asks the page keeps it loading, though the page then says
    /// complete. (Only the outcome is checked: the tracker must not publish
    /// "loaded" before it takes the start, but on this test's current-thread
    /// runtime, no waiter could run in between anyway.)
    #[tokio::test]
    async fn a_start_during_the_question_beats_the_answer() {
        let (mut mock, conn) = MockConnection::pair();
        let tracker = LoadTracker::current(&SessionHandle::new(conn.clone(), "S1"), "F");
        answer(&mut mock, "Page.enable", json!({})).await;
        let id = tokio::time::timeout(LONG, mock.expect_cmd("Runtime.evaluate"))
            .await
            .expect("no question");
        // Chrome sends the event before the reply.
        emit(&mock, true, "F", "S1").await;
        mock.reply(id, json!({ "result": { "value": "complete" } }))
            .await;
        let res = tracker.wait(SHORT).await;
        assert!(matches!(res, Err(ZendriverError::Timeout(_))), "{res:?}");
        conn.shutdown();
    }

    /// When the page can't answer, the frame counts as loading until a stop
    /// arrives.
    #[tokio::test]
    async fn a_failed_question_waits_for_a_stop() {
        let (mut mock, conn) = MockConnection::pair();
        let tracker = LoadTracker::current(&SessionHandle::new(conn.clone(), "S1"), "F");
        let id = tokio::time::timeout(LONG, mock.expect_cmd("Page.enable"))
            .await
            .expect("no question");
        mock.reply_err(id, -32000, "No session with given id").await;
        let waiting = tokio::spawn(tracker.wait(LONG));
        tokio::time::sleep(SHORT).await;
        assert!(!waiting.is_finished(), "loaded without an answer or a stop");
        emit(&mock, false, "F", "S1").await;
        waiting.await.unwrap().expect("loaded after it stopped");
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

    /// And when Chrome detaches the session (as `Tab::close` makes it do to
    /// a tab on the browser's socket).
    #[tokio::test]
    async fn wait_ends_when_chrome_detaches_the_session() {
        let (mock, conn) = MockConnection::pair();
        let tracker = LoadTracker::before_navigating(&SessionHandle::new(conn, "S1"), "F");
        mock.emit_event("Target.detachedFromTarget", json!({ "sessionId": "S1" }))
            .await;
        let res = tokio::time::timeout(LONG, tracker.wait(LONG))
            .await
            .unwrap();
        assert!(matches!(res, Err(ZendriverError::Navigation(_))), "{res:?}");
    }

    /// And when a reconnect moves the connection to a new socket, where the
    /// session doesn't exist.
    #[tokio::test]
    async fn wait_ends_at_a_reconnect() {
        let (mut mock, conn) = MockConnection::pair();
        let tracker = LoadTracker::before_navigating(&SessionHandle::new(conn.clone(), "S1"), "F");
        mock.reconnect(&conn);
        let res = tokio::time::timeout(LONG, tracker.wait(LONG))
            .await
            .unwrap();
        assert!(matches!(res, Err(ZendriverError::Navigation(_))), "{res:?}");
    }
}
