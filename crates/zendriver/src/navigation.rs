//! Waiting for a navigation of the main frame that zendriver didn't start:
//! one that a click, a key press or the page's own script starts. The entry
//! points are [`Tab::expect_navigation`] and [`Tab::wait_for_url`].
//!
//! Chrome announces each navigation of a frame when it commits:
//! `Page.frameNavigated` when a new document replaces the old one (also on
//! a reload, and on a back-forward-cache restore), and
//! `Page.navigatedWithinDocument` when the document stays and only its URL
//! changes (a fragment, or the History API). A new document then loads
//! until `Page.frameStoppedLoading`. The events need the Page domain, which
//! every [`Tab`] enables when it is created. They come from a broadcast
//! channel that never replays an event sent before subscribing, so an
//! expectation subscribes when it is created, before the caller acts.
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
use tokio::time::Sleep;
use url::Url;
use zendriver_transport::{AccountedRawEvent, SessionHandle};

use crate::error::{Result, ZendriverError};
use crate::load_tracker::LoadTracker;
use crate::url_matcher::UrlMatcher;

/// How long a [`NavigationExpectation`] waits unless told otherwise.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Where a wait for a navigation ended: the result of
/// [`Tab::expect_navigation`](crate::Tab::expect_navigation) and
/// [`Tab::wait_for_url`](crate::Tab::wait_for_url).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Navigation {
    /// The main frame's URL when the wait ended, fragment included.
    pub url: Url,
    /// Whether the document that was there when the wait began is still
    /// there: the navigation changed only the URL's fragment or the history
    /// entry (the History API), or, for `wait_for_url`, the tab was already
    /// at a matching URL. The page's state and its element handles survived
    /// then. `false` when a new document loaded.
    pub same_document: bool,
}

/// What the events of the main frame say.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Change {
    /// A navigation committed: the frame is at `url` now. `same_document`
    /// is as in [`Navigation`].
    Committed { url: String, same_document: bool },
    /// The frame finished loading.
    Stopped,
    /// Events may have been missed: the subscriber lagged, or the
    /// connection was replaced.
    Lost,
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
/// receives from now on, in the order Chrome sent them. Ends when the socket
/// dies, but not on a clean [`zendriver_transport::Connection::shutdown`],
/// which the event bus can't tell from a reconnect.
fn changes(session: &SessionHandle, frame_id: &str) -> Pin<Box<dyn Stream<Item = Change> + Send>> {
    let session_id = session.session_id().map(str::to_owned);
    let frame_id = frame_id.to_owned();
    // One subscription for every method, so the events can't swap places
    // on the way here.
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
                    AccountedRawEvent::Event { event, .. } if event.session_id == session_id => {
                        let params = &event.params;
                        match event.method.as_str() {
                            "Page.frameNavigated" if params["frame"]["id"] == frame_id => {
                                navigated_url(params).map(|url| Change::Committed {
                                    url,
                                    same_document: false,
                                })
                            }
                            "Page.navigatedWithinDocument" if params["frameId"] == frame_id => {
                                params["url"].as_str().map(|url| Change::Committed {
                                    url: url.to_owned(),
                                    same_document: true,
                                })
                            }
                            "Page.frameStoppedLoading" if params["frameId"] == frame_id => {
                                Some(Change::Stopped)
                            }
                            _ => None,
                        }
                    }
                    AccountedRawEvent::Event { .. } | AccountedRawEvent::Disconnected { .. } => {
                        None
                    }
                    AccountedRawEvent::Lagged { .. } | AccountedRawEvent::Reconnected { .. } => {
                        Some(Change::Lost)
                    }
                };
                async move { change }
            }),
    )
}

/// The main frame's URL now, fragment included, from the browser process.
async fn current_url(session: &SessionHandle) -> Result<String> {
    let reply = session.call("Target.getTargetInfo", json!({})).await?;
    let url = reply["targetInfo"]["url"].as_str();
    url.map(str::to_owned)
        .ok_or_else(|| ZendriverError::Navigation("Target.getTargetInfo returned no url".into()))
}

/// A [`Navigation`] that ended at `url`.
fn navigation(url: &str, same_document: bool) -> Result<Navigation> {
    let url = Url::parse(url).map_err(|e| {
        ZendriverError::Navigation(format!("the main frame's URL {url:?} doesn't parse: {e}"))
    })?;
    Ok(Navigation { url, same_document })
}

/// The document that the main frame `frame_id` on `session` shows now, as
/// the [`Navigation`] a wait can end on without any navigation: `Some` once
/// it has loaded, if its URL `matches` both before and after the load (the
/// frame may move on meanwhile).
async fn current_if_matching(
    session: &SessionHandle,
    frame_id: &str,
    matches: &impl Fn(&str) -> bool,
) -> Result<Option<Navigation>> {
    if !matches(&current_url(session).await?) {
        return Ok(None);
    }
    // The wait's own timeout bounds this.
    LoadTracker::current(session, frame_id)
        .wait(Duration::MAX)
        .await?;
    let url = current_url(session).await?;
    if !matches(&url) {
        return Ok(None);
    }
    navigation(&url, true).map(Some)
}

/// What a [`NavigationExpectation`] waits for, fixed when it is created.
struct Armed {
    session: SessionHandle,
    frame_id: String,
    changes: Pin<Box<dyn Stream<Item = Change> + Send>>,
    /// Only a navigation to a URL matching this counts; `None` for any.
    to: Option<UrlMatcher>,
    /// Whether the document the tab shows when the wait begins counts too,
    /// if its URL matches (`wait_for_url`). Lost events then make the wait
    /// read the URL again instead of ending.
    accept_current: bool,
}

/// Wait as `armed` says, with no time limit.
async fn wait(armed: Armed) -> Result<Navigation> {
    let Armed {
        session,
        frame_id,
        mut changes,
        to,
        accept_current,
    } = armed;
    let matches = |url: &str| to.as_ref().is_none_or(|to| to.matches(url));
    if accept_current {
        if let Some(found) = current_if_matching(&session, &frame_id, &matches).await? {
            return Ok(found);
        }
    }
    // The URL of a matching new document that is loading.
    let mut loading: Option<String> = None;
    while let Some(change) = changes.next().await {
        match change {
            Change::Committed { url, same_document } => {
                if !matches(&url) {
                    loading = None;
                } else if same_document && loading.is_none() {
                    return navigation(&url, true);
                } else {
                    // A new document; or the loading one changed its URL.
                    loading = Some(url);
                }
            }
            Change::Stopped => {
                if let Some(url) = loading.take() {
                    return navigation(&url, false);
                }
            }
            Change::Lost => {
                if !accept_current {
                    return Err(ZendriverError::EventStreamIncomplete);
                }
                if let Some(found) = current_if_matching(&session, &frame_id, &matches).await? {
                    return Ok(found);
                }
                loading = None;
            }
        }
    }
    Err(ZendriverError::Navigation(
        "the page's connection closed while waiting for a navigation".into(),
    ))
}

/// Awaitable handle returned by
/// [`Tab::expect_navigation`](crate::Tab::expect_navigation) and
/// [`Tab::wait_for_url`](crate::Tab::wait_for_url): resolves with the
/// [`Navigation`] the wait ended on, or with [`ZendriverError::Timeout`]
/// after 30 s unless [`NavigationExpectation::timeout`] says otherwise.
///
/// Subscribes to the main frame's events when created, so a navigation
/// after that can't be missed, however long it takes until the first
/// `.await`.
pub struct NavigationExpectation {
    wait: Pin<Box<dyn Future<Output = Result<Navigation>> + Send>>,
    timeout: Duration,
    /// Armed by the first poll, so that [`Self::timeout`] applies.
    sleep: Option<Pin<Box<Sleep>>>,
}

impl NavigationExpectation {
    /// Wait for a navigation of the main frame `frame_id` on `session` to a
    /// URL matching `to` (any URL if `None`); or, when `accept_current`,
    /// for the frame to be at such a URL, which it may be already.
    pub(crate) fn new(
        session: &SessionHandle,
        frame_id: &str,
        to: Option<UrlMatcher>,
        accept_current: bool,
    ) -> Self {
        let armed = Armed {
            session: session.clone(),
            frame_id: frame_id.to_owned(),
            changes: changes(session, frame_id),
            to,
            accept_current,
        };
        Self {
            wait: Box::pin(wait(armed)),
            timeout: DEFAULT_TIMEOUT,
            sleep: None,
        }
    }

    /// Override the default 30 s timeout. Call it before the first `.await`.
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
    #[must_use]
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

impl Future for NavigationExpectation {
    type Output = Result<Navigation>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Poll::Ready(res) = self.wait.as_mut().poll(cx) {
            return Poll::Ready(res);
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

    /// Answer the expectation's question about the current URL with `url`.
    async fn answer_url(mock: &mut MockConnection, url: &str) {
        let info = json!({ "targetInfo": { "url": url } });
        answer(mock, "Target.getTargetInfo", info).await;
    }

    /// Answer the probe of the current document's load state with
    /// `ready_state`.
    async fn answer_probe(mock: &mut MockConnection, ready_state: &str) {
        answer(mock, "Page.enable", json!({})).await;
        let value = json!({ "result": { "value": ready_state } });
        answer(mock, "Runtime.evaluate", value).await;
    }

    /// Return once every frame emitted or replied so far has reached `conn`:
    /// a reconnect drops the frames the old socket hasn't delivered yet.
    /// (Frames arrive in order, so an unrelated event emitted now arrives
    /// after them.)
    async fn delivered(mock: &MockConnection, conn: &Connection) {
        let mut seen = conn.subscribe_raw_accounted();
        mock.emit_event_for_session("Page.frameStoppedLoading", json!({ "frameId": "G" }), "S1")
            .await;
        seen.next().await;
    }

    fn expect_navigation(conn: &Connection) -> NavigationExpectation {
        NavigationExpectation::new(&SessionHandle::new(conn.clone(), "S1"), "F", None, false)
    }

    fn wait_for_url(conn: &Connection, pattern: &str) -> NavigationExpectation {
        let session = SessionHandle::new(conn.clone(), "S1");
        NavigationExpectation::new(&session, "F", Some(pattern.into()), true)
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
        answer_url(&mut mock, "http://a.test/start").await;
        commit(&mock, "http://a.test/other", false).await;
        stop(&mock).await;
        commit(&mock, "http://a.test/target?replaced", false).await;
        commit(&mock, "http://a.test/elsewhere", false).await;
        stop(&mock).await;
        let early = tokio::time::timeout(SHORT, &mut waiting).await;
        assert!(early.is_err(), "ended on a page at another URL: {early:?}");
        commit(&mock, "http://a.test/target", false).await;
        stop(&mock).await;
        let found = waiting.await.unwrap().expect("the target loaded");
        assert_eq!(found, reached("http://a.test/target", false));
        conn.shutdown();
    }

    /// A tab already at a matching URL ends the wait as soon as its page
    /// has loaded: at once if it has, else at the page's stop.
    #[tokio::test]
    async fn wait_for_url_ends_on_the_page_already_at_the_url() {
        for (ready_state, stops) in [("complete", false), ("loading", true)] {
            let (mut mock, conn) = MockConnection::pair();
            let waiting = tokio::spawn(wait_for_url(&conn, "/target"));
            answer_url(&mut mock, "http://a.test/target").await;
            answer_probe(&mut mock, ready_state).await;
            if stops {
                stop(&mock).await;
            }
            answer_url(&mut mock, "http://a.test/target").await;
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

    /// After lost events, `expect_navigation` can't tell whether the
    /// navigation happened, so it ends; `wait_for_url` reads the URL again.
    #[tokio::test]
    async fn lost_events_end_expect_navigation_and_make_wait_for_url_look_again() {
        let (mut mock, conn) = MockConnection::pair();
        let waiting = tokio::spawn(expect_navigation(&conn));
        mock.reconnect(&conn);
        let res = waiting.await.unwrap();
        assert!(
            matches!(res, Err(ZendriverError::EventStreamIncomplete)),
            "{res:?}"
        );
        conn.shutdown();

        let (mut mock, conn) = MockConnection::pair();
        let waiting = tokio::spawn(wait_for_url(&conn, "/target"));
        answer_url(&mut mock, "http://a.test/start").await;
        delivered(&mock, &conn).await;
        mock.reconnect(&conn);
        answer_url(&mut mock, "http://a.test/target").await;
        answer_probe(&mut mock, "complete").await;
        answer_url(&mut mock, "http://a.test/target").await;
        let found = waiting
            .await
            .unwrap()
            .expect("found at the URL after lost events");
        assert_eq!(found, reached("http://a.test/target", true));
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
}
