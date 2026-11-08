//! Waiting for the main frame to navigate, or to be at a URL. The entry
//! points are [`Tab::expect_navigation`], for a navigation that a click, a
//! key press or the page's own script starts, and [`Tab::wait_for_url`],
//! for a destination, whoever starts the navigation there.
//!
//! Chrome announces each navigation of a frame when it commits:
//! `Page.frameNavigated` when a new document replaces the old one (also on
//! a reload, on a back-forward-cache restore, and for the error page of a
//! navigation that failed), and `Page.navigatedWithinDocument` when the
//! document stays and only its URL changes (a fragment, or the History
//! API). A new document then loads until `Page.frameStoppedLoading`. The
//! events need the Page domain, which every [`Tab`] enables when it is
//! created. A subscription gets the events that arrive after it, all of
//! them and in order ([`SessionHandle::events`]), so an expectation
//! subscribes when it is created, before the caller acts, and reads the
//! events in a task of its own from then on.
//!
//! Where the frame is now, and whether its document has loaded, only the
//! page can say: `Target.getTargetInfo` reports a navigation's URL while
//! it is still pending, and the URL a failed navigation was to. So a wait
//! for a URL asks the page for `location.href` and `document.readyState`,
//! together, in one `Runtime.evaluate`.
//!
//! [`Tab`]: crate::Tab
//! [`Tab::expect_navigation`]: crate::Tab::expect_navigation
//! [`Tab::wait_for_url`]: crate::Tab::wait_for_url

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio::time::Sleep;
use tracing::warn;
use url::Url;
use zendriver_transport::{SessionEnd, SessionHandle};

use crate::error::{Result, ZendriverError};
use crate::url_matcher::UrlMatcher;

/// How long a [`NavigationExpectation`] waits unless told otherwise.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The scheme of Chrome's error page, shown for a navigation that failed.
const ERROR_PAGE: &str = "chrome-error://";

/// Where a wait for a navigation ended: the result of
/// [`Tab::expect_navigation`](crate::Tab::expect_navigation) and
/// [`Tab::wait_for_url`](crate::Tab::wait_for_url).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Navigation {
    /// The main frame's URL when the wait ended, fragment included.
    pub url: Url,
    /// Whether the document that was there when the wait began is still
    /// there, so that the page's state and its element handles survived:
    /// `true` when every navigation the wait saw stayed in that document
    /// (a fragment change, or the History API), or when the wait saw no
    /// navigation because the tab was already at the URL. `false` when a
    /// new document replaced it.
    pub same_document: bool,
}

/// What the events of the main frame say.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Change {
    /// A new document committed: the frame is at `url` now, loading it.
    NewDocument { url: String },
    /// Chrome's error page committed, for a navigation to `url` that failed.
    ErrorPage { url: String },
    /// The document stayed, and its URL changed to `url`.
    SameDocument { url: String },
    /// The frame finished loading.
    Stopped,
    /// The session's events ended, for this reason: the last change.
    Ended(SessionEnd),
}

/// The frame's URL in the `params` of a `Page.frameNavigated`, fragment
/// included (Chrome reports the fragment apart from the rest).
fn navigated_url(params: &Value) -> Option<String> {
    let frame = &params["frame"];
    let url = frame["url"].as_str()?;
    let fragment = frame["urlFragment"].as_str().unwrap_or("");
    Some(format!("{url}{fragment}"))
}

/// The changes of the main frame, whose id is `frame_id`, that `session`
/// receives from now on, in the order Chrome sent them, ending with
/// [`Change::Ended`].
fn changes(session: &SessionHandle, frame_id: &str) -> Pin<Box<dyn Stream<Item = Change> + Send>> {
    let frame_id = frame_id.to_owned();
    // One subscription for every method, so the events can't swap places
    // on the way here.
    let events = session.events(&[
        "Page.frameNavigated",
        "Page.navigatedWithinDocument",
        "Page.frameStoppedLoading",
    ]);
    Box::pin(events.filter_map(move |event| {
        let event = match event {
            Ok(event) => event,
            Err(end) => return std::future::ready(Some(Change::Ended(end))),
        };
        let params = &event.params;
        let change = match event.method.as_str() {
            "Page.frameNavigated" if params["frame"]["id"] == frame_id => {
                let failed = params["frame"]["unreachableUrl"].as_str();
                match failed.filter(|url| !url.is_empty()) {
                    Some(url) => Some(Change::ErrorPage {
                        url: url.to_owned(),
                    }),
                    None => navigated_url(params).map(|url| Change::NewDocument { url }),
                }
            }
            "Page.navigatedWithinDocument" if params["frameId"] == frame_id => {
                params["url"].as_str().map(|url| Change::SameDocument {
                    url: url.to_owned(),
                })
            }
            "Page.frameStoppedLoading" if params["frameId"] == frame_id => Some(Change::Stopped),
            _ => None,
        };
        std::future::ready(change)
    }))
}

/// A [`Navigation`] that ended at `url`.
fn navigation(url: &str, same_document: bool) -> Result<Navigation> {
    let url = Url::parse(url).map_err(|e| {
        ZendriverError::Navigation(format!("the main frame's URL {url:?} doesn't parse: {e}"))
    })?;
    Ok(Navigation { url, same_document })
}

/// The error of a navigation to `url` that ended on Chrome's error page.
fn failed(url: &str) -> ZendriverError {
    ZendriverError::Navigation(format!("the navigation to {url} failed"))
}

/// The error of a wait whose session's events ended, for the reason `end`.
fn ended(end: SessionEnd) -> ZendriverError {
    match end {
        SessionEnd::Overflowed => ZendriverError::EventStreamIncomplete,
        _ => ZendriverError::Navigation(format!(
            "the tab's session or connection ended while waiting for a navigation: {end}"
        )),
    }
}

/// Where the page is and whether it has loaded, as the page itself says:
/// `location.href` (fragment included) and `document.readyState`. `None`
/// if the page can't answer, which is logged.
async fn ask_page(session: &SessionHandle) -> Option<(String, String)> {
    let params = json!({
        "expression": "[location.href, document.readyState]",
        "returnByValue": true,
    });
    let reply = session.call("Runtime.evaluate", params).await;
    let pair = reply.as_ref().ok().and_then(|reply| {
        let value = &reply["result"]["value"];
        Some((value[0].as_str()?.to_owned(), value[1].as_str()?.to_owned()))
    });
    if pair.is_none() {
        warn!(
            ?reply,
            "the page didn't say where it is; going by the events"
        );
    }
    pair
}

/// The URL that the failed navigation behind the error page in the main
/// frame was to, if Chrome tells.
async fn unreachable_url(session: &SessionHandle) -> Option<String> {
    let reply = session.call("Page.getFrameTree", json!({})).await.ok()?;
    let url = reply["frameTree"]["frame"]["unreachableUrl"].as_str()?;
    (!url.is_empty()).then(|| url.to_owned())
}

/// What a [`NavigationExpectation`] waits for.
pub(crate) enum Target {
    /// The next navigation of the main frame, wherever it leads.
    NextNavigation,
    /// The main frame being at a URL matching this, which it may be
    /// already.
    Url(UrlMatcher),
}

impl Target {
    /// Whether a navigation to `url`, or being at `url`, is what the wait
    /// is for.
    fn matches(&self, url: &str) -> bool {
        match self {
            Self::NextNavigation => true,
            Self::Url(matcher) => matcher.matches(url),
        }
    }
}

/// The main frame, as far as the wait knows.
#[derive(Default)]
struct Frame {
    /// A new document has committed and hasn't finished loading.
    loading: bool,
    /// A new document has replaced the one the wait began with.
    replaced: bool,
    /// The matching URL the frame is at, to report once its document has
    /// loaded.
    candidate: Option<String>,
}

impl Frame {
    /// The [`Navigation`] to end the wait on, if the frame is at a matching
    /// URL and has loaded.
    fn arrived(&mut self) -> Result<Option<Navigation>> {
        if self.loading {
            return Ok(None);
        }
        let Some(url) = self.candidate.take() else {
            return Ok(None);
        };
        navigation(&url, !self.replaced).map(Some)
    }
}

/// Take what the page says about where it is and whether it has loaded
/// into `frame`, and return the [`Navigation`] to end the wait on, if the
/// frame is at a URL matching `target` and has loaded. If the page can't
/// answer, the events alone decide.
///
/// # Errors
///
/// [`ZendriverError::Navigation`] when the frame shows the error page of a
/// failed navigation to a matching URL.
async fn consult_page(
    session: &SessionHandle,
    target: &Target,
    frame: &mut Frame,
) -> Result<Option<Navigation>> {
    let Some((url, ready_state)) = ask_page(session).await else {
        return frame.arrived();
    };
    frame.loading = ready_state != "complete";
    frame.candidate = None;
    if url.starts_with(ERROR_PAGE) {
        return match unreachable_url(session).await {
            Some(failed_url) if target.matches(&failed_url) => Err(failed(&failed_url)),
            _ => Ok(None),
        };
    }
    frame.candidate = target.matches(&url).then_some(url);
    frame.arrived()
}

/// Wait for `target` on the main frame of `session`, whose `changes` these
/// are, with no time limit.
async fn wait(
    session: SessionHandle,
    mut changes: Pin<Box<dyn Stream<Item = Change> + Send>>,
    target: Target,
) -> Result<Navigation> {
    let asks_page = matches!(target, Target::Url(_));
    let mut frame = Frame::default();
    if asks_page {
        if let Some(found) = consult_page(&session, &target, &mut frame).await? {
            return Ok(found);
        }
    }
    while let Some(change) = changes.next().await {
        match change {
            Change::NewDocument { url } => {
                frame.replaced = true;
                frame.loading = true;
                frame.candidate = target.matches(&url).then_some(url);
            }
            Change::ErrorPage { url } => {
                frame.replaced = true;
                frame.loading = true;
                frame.candidate = None;
                if target.matches(&url) {
                    return Err(failed(&url));
                }
            }
            Change::SameDocument { url } => {
                frame.candidate = target.matches(&url).then_some(url);
            }
            Change::Stopped => {
                frame.loading = false;
            }
            Change::Ended(end) => return Err(ended(end)),
        }
        if frame.loading || frame.candidate.is_none() {
            continue;
        }
        // The events say the frame is at a matching URL and has loaded. For
        // a URL, the page confirms it, since events queued before it last
        // answered can be older than that answer.
        let found = if asks_page {
            consult_page(&session, &target, &mut frame).await?
        } else {
            frame.arrived()?
        };
        if let Some(found) = found {
            return Ok(found);
        }
    }
    // The changes end with `Change::Ended`, which returned above.
    Err(ended(SessionEnd::Shutdown))
}

/// Awaitable handle returned by
/// [`Tab::expect_navigation`](crate::Tab::expect_navigation) and
/// [`Tab::wait_for_url`](crate::Tab::wait_for_url): resolves with the
/// [`Navigation`] the wait ended on, or with [`ZendriverError::Timeout`]
/// after 30 s unless [`NavigationExpectation::timeout`] says otherwise.
///
/// Subscribes to the main frame's events when it is created, and reads
/// them in a task of its own from then on, so a navigation after that can't
/// be missed, however long it takes until the first `.await`. Dropping it
/// ends the task.
#[must_use = "does nothing unless awaited"]
pub struct NavigationExpectation {
    task: JoinHandle<Result<Navigation>>,
    timeout: Duration,
    /// Armed by the first poll, so that [`Self::timeout`] applies.
    sleep: Option<Pin<Box<Sleep>>>,
}

impl NavigationExpectation {
    /// Wait for `target` on the main frame `frame_id` of `session`.
    pub(crate) fn new(session: &SessionHandle, frame_id: &str, target: Target) -> Self {
        let changes = changes(session, frame_id);
        Self {
            task: tokio::spawn(wait(session.clone(), changes, target)),
            timeout: DEFAULT_TIMEOUT,
            sleep: None,
        }
    }

    /// Override the default 30 s timeout. Call it before the first `.await`:
    /// the clock starts at the first poll, not when the expectation is
    /// created.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// // A login done by hand in a headed window can take a while.
    /// tab.wait_for_url("/dashboard")
    ///     .timeout(Duration::from_secs(300))
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub fn timeout(mut self, dur: Duration) -> Self {
        self.timeout = dur;
        self.sleep = None;
        self
    }
}

impl fmt::Debug for NavigationExpectation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NavigationExpectation")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl Drop for NavigationExpectation {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Future for NavigationExpectation {
    type Output = Result<Navigation>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Poll::Ready(res) = Pin::new(&mut self.task).poll(cx) {
            // The task only ends by returning, so this is its result.
            return Poll::Ready(res.unwrap_or_else(|e| {
                Err(ZendriverError::Navigation(format!(
                    "the wait for a navigation ended unexpectedly: {e}"
                )))
            }));
        }
        let timeout = self.timeout;
        let sleep = self
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
        match sleep.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(ZendriverError::Timeout(timeout))),
            Poll::Pending => Poll::Pending,
        }
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

    /// Emit a commit of frame `F` on session `S1`, to `url`.
    async fn commit(mock: &MockConnection, url: &str, same_document: bool) {
        if same_document {
            let params = json!({ "frameId": "F", "url": url, "navigationType": "historyApi" });
            mock.emit_event_for_session("Page.navigatedWithinDocument", params, "S1")
                .await;
        } else {
            let (url, fragment) = url
                .split_once('#')
                .map_or((url, None), |(url, f)| (url, Some(format!("#{f}"))));
            let frame = json!({ "id": "F", "url": url, "urlFragment": fragment, "loaderId": "L" });
            let params = json!({ "frame": frame, "type": "Navigation" });
            mock.emit_event_for_session("Page.frameNavigated", params, "S1")
                .await;
        }
    }

    /// Emit a stop of frame `F` on session `S1`.
    async fn stop(mock: &MockConnection) {
        mock.emit_event_for_session("Page.frameStoppedLoading", json!({ "frameId": "F" }), "S1")
            .await;
    }

    /// Wait for the expectation to send `method`, and reply `reply`.
    async fn answer(mock: &mut MockConnection, method: &str, reply: Value) {
        let id = tokio::time::timeout(LONG, mock.expect_cmd(method))
            .await
            .unwrap_or_else(|_| panic!("the wait didn't send {method}"));
        mock.reply(id, reply).await;
    }

    /// Emit the commit of Chrome's error page in frame `F` on session `S1`,
    /// for a navigation to `url` that failed.
    async fn error_page(mock: &MockConnection, url: &str) {
        let frame =
            json!({ "id": "F", "url": "chrome-error://chromewebdata/", "unreachableUrl": url });
        mock.emit_event_for_session("Page.frameNavigated", json!({ "frame": frame }), "S1")
            .await;
    }

    /// Answer the wait's question to the page: it is at `url`, and its
    /// `document.readyState` is `ready_state`.
    async fn answer_page(mock: &mut MockConnection, url: &str, ready_state: &str) {
        let value = json!({ "result": { "value": [url, ready_state] } });
        answer(mock, "Runtime.evaluate", value).await;
    }

    fn expect_navigation(conn: &Connection) -> NavigationExpectation {
        let session = SessionHandle::new(conn.clone(), "S1");
        NavigationExpectation::new(&session, "F", Target::NextNavigation)
    }

    fn wait_for_url(conn: &Connection, pattern: &str) -> NavigationExpectation {
        let session = SessionHandle::new(conn.clone(), "S1");
        NavigationExpectation::new(&session, "F", Target::Url(pattern.into()))
    }

    fn reached(url: &str, same_document: bool) -> Navigation {
        Navigation {
            url: Url::parse(url).unwrap(),
            same_document,
        }
    }

    /// A new document ends the wait once it has loaded, at the URL it
    /// committed with, fragment included.
    #[tokio::test]
    async fn expect_navigation_ends_when_the_new_document_has_loaded() {
        let (mock, conn) = MockConnection::pair();
        let mut waiting = tokio::spawn(expect_navigation(&conn));
        commit(&mock, "http://a.test/page#top", false).await;
        let early = tokio::time::timeout(SHORT, &mut waiting).await;
        assert!(
            early.is_err(),
            "ended before the document loaded: {early:?}"
        );
        stop(&mock).await;
        let found = waiting.await.unwrap().expect("loaded");
        assert_eq!(found, reached("http://a.test/page#top", false));
        conn.shutdown();
    }

    /// A navigation that stays in the document ends the wait at once.
    #[tokio::test]
    async fn a_same_document_navigation_ends_the_wait_at_once() {
        let (mock, conn) = MockConnection::pair();
        let waiting = expect_navigation(&conn);
        commit(&mock, "http://a.test/page?pushed", true).await;
        let found = tokio::time::timeout(LONG, waiting)
            .await
            .unwrap()
            .expect("same document");
        assert_eq!(found, reached("http://a.test/page?pushed", true));
        conn.shutdown();
    }

    /// Other frames' commits, other sessions' commits, and a stop with no
    /// commit before it don't count.
    #[tokio::test]
    async fn only_the_main_frames_own_navigation_counts() {
        let (mock, conn) = MockConnection::pair();
        let waiting = expect_navigation(&conn).timeout(SHORT);
        let other_frame = json!({ "frame": { "id": "G", "url": "http://a.test/other" } });
        mock.emit_event_for_session("Page.frameNavigated", other_frame, "S1")
            .await;
        let other_session = json!({ "frame": { "id": "F", "url": "http://a.test/other" } });
        mock.emit_event_for_session("Page.frameNavigated", other_session, "S2")
            .await;
        stop(&mock).await;
        let res = waiting.await;
        assert!(matches!(res, Err(ZendriverError::Timeout(_))), "{res:?}");
        conn.shutdown();
    }

    /// `wait_for_url` skips navigations to other URLs, and a matching
    /// document that another replaces before it has loaded.
    #[tokio::test]
    async fn wait_for_url_ends_on_the_first_matching_document_that_loads() {
        let (mut mock, conn) = MockConnection::pair();
        let mut waiting = tokio::spawn(wait_for_url(&conn, "/target"));
        answer_page(&mut mock, "http://a.test/start", "complete").await;
        commit(&mock, "http://a.test/other", false).await;
        stop(&mock).await;
        commit(&mock, "http://a.test/target?replaced", false).await;
        commit(&mock, "http://a.test/elsewhere", false).await;
        stop(&mock).await;
        let early = tokio::time::timeout(SHORT, &mut waiting).await;
        assert!(early.is_err(), "ended on a page at another URL: {early:?}");
        commit(&mock, "http://a.test/target", false).await;
        stop(&mock).await;
        // The page confirms what the events said.
        answer_page(&mut mock, "http://a.test/target", "complete").await;
        let found = waiting.await.unwrap().expect("the target loaded");
        assert_eq!(found, reached("http://a.test/target", false));
        conn.shutdown();
    }

    /// A tab already at a matching URL ends the wait as soon as its page
    /// has loaded: at once if it has, else at the page's stop.
    #[tokio::test]
    async fn wait_for_url_ends_on_the_page_already_at_the_url() {
        for (ready_state, stops) in [("complete", false), ("interactive", true)] {
            let (mut mock, conn) = MockConnection::pair();
            let waiting = tokio::spawn(wait_for_url(&conn, "/target"));
            answer_page(&mut mock, "http://a.test/target", ready_state).await;
            if stops {
                stop(&mock).await;
                answer_page(&mut mock, "http://a.test/target", "complete").await;
            }
            let found = waiting
                .await
                .unwrap()
                .unwrap_or_else(|e| panic!("{ready_state}: {e}"));
            assert_eq!(
                found,
                reached("http://a.test/target", true),
                "{ready_state}"
            );
            conn.shutdown();
        }
    }

    /// A reconnect moves the connection to a new socket, where the session
    /// doesn't exist, so both kinds of wait end, saying so.
    #[tokio::test]
    async fn a_reconnect_ends_the_wait() {
        let (mut mock, conn) = MockConnection::pair();
        let next = tokio::spawn(expect_navigation(&conn));
        let url = tokio::spawn(wait_for_url(&conn, "/target"));
        answer_page(&mut mock, "http://a.test/start", "complete").await;
        mock.reconnect(&conn);
        for waiting in [next, url] {
            let res = waiting.await.unwrap();
            assert!(
                matches!(&res, Err(ZendriverError::Navigation(m)) if m.contains("new socket")),
                "{res:?}"
            );
        }
        conn.shutdown();
    }

    /// A wait that falls so far behind its events that its queue overflows
    /// can't tell what it missed.
    #[tokio::test]
    async fn an_overflow_ends_the_wait_as_incomplete() {
        let (mock, conn) = MockConnection::pair_with_session_queue_capacity(1);
        let session = SessionHandle::new(conn.clone(), "S1");
        let changes = changes(&session, "F");
        // The actor fills the queues before it broadcasts an event, so once
        // the broadcast has both stops, the second has overflowed the queue.
        let mut broadcast = conn.subscribe_raw();
        stop(&mock).await;
        stop(&mock).await;
        broadcast.next().await;
        broadcast.next().await;
        let res = wait(session, changes, Target::NextNavigation).await;
        assert!(
            matches!(res, Err(ZendriverError::EventStreamIncomplete)),
            "{res:?}"
        );
        conn.shutdown();
    }

    /// A URL change within a document that is still loading waits for that
    /// document's load, though the document committed at a URL that didn't
    /// match.
    #[tokio::test]
    async fn a_url_change_in_a_loading_document_waits_for_its_load() {
        let (mut mock, conn) = MockConnection::pair();
        let mut waiting = tokio::spawn(wait_for_url(&conn, "/target"));
        answer_page(&mut mock, "http://a.test/start", "complete").await;
        commit(&mock, "http://a.test/app", false).await;
        commit(&mock, "http://a.test/app/target", true).await;
        let early = tokio::time::timeout(SHORT, &mut waiting).await;
        assert!(
            early.is_err(),
            "ended before the document loaded: {early:?}"
        );
        stop(&mock).await;
        answer_page(&mut mock, "http://a.test/app/target", "complete").await;
        let found = waiting.await.unwrap().expect("loaded");
        assert_eq!(found, reached("http://a.test/app/target", false));
        conn.shutdown();
    }

    /// Events queued while the page was asked where it is can be older than
    /// its answer: the page has the last word.
    #[tokio::test]
    async fn wait_for_url_trusts_the_page_over_older_events() {
        let (mut mock, conn) = MockConnection::pair();
        let mut waiting = tokio::spawn(wait_for_url(&conn, "/target"));
        commit(&mock, "http://a.test/target", false).await;
        stop(&mock).await;
        // The frame has moved on since those events, the page says, twice:
        // once asked at the start, once asked to confirm them.
        answer_page(&mut mock, "http://a.test/final", "complete").await;
        answer_page(&mut mock, "http://a.test/final", "complete").await;
        let early = tokio::time::timeout(SHORT, &mut waiting).await;
        assert!(
            early.is_err(),
            "ended on a URL the frame had left: {early:?}"
        );
        commit(&mock, "http://a.test/target", false).await;
        stop(&mock).await;
        answer_page(&mut mock, "http://a.test/target", "complete").await;
        let found = waiting.await.unwrap().expect("the target loaded");
        assert_eq!(found, reached("http://a.test/target", false));
        conn.shutdown();
    }

    /// A navigation that ends on Chrome's error page fails the wait for its
    /// URL, whether the events or the page tell of it; another URL's
    /// failure is skipped.
    #[tokio::test]
    async fn a_failed_navigation_fails_the_wait() {
        let (mock, conn) = MockConnection::pair();
        let waiting = expect_navigation(&conn);
        error_page(&mock, "http://127.0.0.1:9/nope").await;
        let res = tokio::time::timeout(LONG, waiting).await.unwrap();
        assert!(
            matches!(&res, Err(ZendriverError::Navigation(m)) if m.contains("/nope")),
            "{res:?}"
        );
        conn.shutdown();

        let (mut mock, conn) = MockConnection::pair();
        let mut waiting = tokio::spawn(wait_for_url(&conn, "/target"));
        answer_page(&mut mock, "http://a.test/start", "complete").await;
        error_page(&mock, "http://a.test/other").await;
        let early = tokio::time::timeout(SHORT, &mut waiting).await;
        assert!(early.is_err(), "ended on another URL's failure: {early:?}");
        error_page(&mock, "http://a.test/target").await;
        let res = waiting.await.unwrap();
        assert!(
            matches!(&res, Err(ZendriverError::Navigation(m)) if m.contains("/target")),
            "{res:?}"
        );
        conn.shutdown();

        let (mut mock, conn) = MockConnection::pair();
        let waiting = tokio::spawn(wait_for_url(&conn, "/target"));
        answer_page(&mut mock, "chrome-error://chromewebdata/", "complete").await;
        let frame =
            json!({ "frameTree": { "frame": { "unreachableUrl": "http://a.test/target" } } });
        answer(&mut mock, "Page.getFrameTree", frame).await;
        let res = waiting.await.unwrap();
        assert!(
            matches!(&res, Err(ZendriverError::Navigation(m)) if m.contains("/target")),
            "{res:?}"
        );
        conn.shutdown();
    }

    /// The wait ends at its timeout, and at once when the socket dies.
    #[tokio::test]
    async fn the_wait_ends_at_its_timeout_or_when_the_connection_dies() {
        let (mock, conn) = MockConnection::pair();
        let res = expect_navigation(&conn).timeout(SHORT).await;
        assert!(
            matches!(res, Err(ZendriverError::Timeout(d)) if d == SHORT),
            "{res:?}"
        );
        let waiting = expect_navigation(&conn);
        mock.disconnect();
        let res = tokio::time::timeout(LONG, waiting).await.unwrap();
        assert!(matches!(res, Err(ZendriverError::Navigation(_))), "{res:?}");
    }

    /// And at once when the connection is shut down (as `Tab::close` does
    /// to a tab that has its own socket).
    #[tokio::test]
    async fn the_wait_ends_when_the_connection_is_shut_down() {
        let (_mock, conn) = MockConnection::pair();
        let waiting = expect_navigation(&conn);
        conn.shutdown();
        let res = tokio::time::timeout(LONG, waiting).await.unwrap();
        assert!(matches!(res, Err(ZendriverError::Navigation(_))), "{res:?}");
    }

    /// And at once when Chrome detaches the session (as `Tab::close` makes
    /// it do to a tab on the browser's socket).
    #[tokio::test]
    async fn the_wait_ends_when_chrome_detaches_the_session() {
        let (mock, conn) = MockConnection::pair();
        let waiting = expect_navigation(&conn);
        mock.emit_event("Target.detachedFromTarget", json!({ "sessionId": "S1" }))
            .await;
        let res = tokio::time::timeout(LONG, waiting).await.unwrap();
        assert!(matches!(res, Err(ZendriverError::Navigation(_))), "{res:?}");
    }
}
