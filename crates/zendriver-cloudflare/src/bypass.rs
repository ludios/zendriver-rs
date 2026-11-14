//! Cloudflare Turnstile bypass driver.
//!
//! Public entry is [`CloudflareBypass`] — constructed via
//! `Tab::cloudflare()` (zendriver crate, feature-gated). The driver runs a
//! single CDP poll loop that, per tick:
//!
//! 1. Observes the page. One evaluator, run in an isolated world where the
//!    page cannot see it, reads the light DOM for the clearance token (one
//!    of the [`token_inputs`](TurnstileSelectors::token_inputs) holding a
//!    value) and for the container and token-input markers. CDP then finds
//!    the widget's `<iframe>`, which Turnstile mounts in a *closed* shadow
//!    root that no path through the DOM leads into. See
//!    [`detection`](crate::detection) for how.
//! 2. Resolves to [`ClearanceOutcome::TokenAcquired`] the first tick a
//!    non-empty token is observed — including the **invisible Turnstile**
//!    path, where nothing is clicked and the token is populated directly.
//! 3. If a click is due and the widget is mounted, scrolls it into view
//!    and measures it; if it is a click target (non-zero size and visible),
//!    clicks the point the [`ClickPolicy`] chose inside it — by default
//!    `bbox.x + bbox.width * 0.07, bbox.y + bbox.height * 0.50`. Up to
//!    [`ClickPolicy::max_attempts`] clicks are spent, spaced
//!    [`ClickPolicy::retry_ticks`] ticks apart, so a swallowed first click
//!    is not the end of the run. A caller-supplied
//!    [`on_click`](CloudflareBypass::on_click) handler replaces the built-in
//!    raw mouse dispatch.
//! 4. Resolves to [`ClearanceOutcome::ChallengeGone`] when every marker
//!    seen on an earlier tick is gone *and the page has settled*: either a
//!    navigation replaced the document and the new one has been parsed, or
//!    the markers stayed gone for three seconds on the same document.
//!    Cloudflare's full-page challenge removes its widget and its token
//!    input a second or two before it navigates on, and its pages keep
//!    another marker (a script) until the page it guards replaces them;
//!    returning on the widget and the input alone would hand the caller a
//!    page of Cloudflare's.
//! 5. Resolves to [`ClearanceOutcome::TimedOut`] on deadline, carrying
//!    `saw_challenge` — `true` when challenge markers were seen but never
//!    resolved, `false` when the entire timeout window elapsed without
//!    observing any challenge markers (the caller likely invoked the bypass
//!    on a page that has no Cloudflare gate at all).
//!
//! Every marker the evaluators look for is caller data — see
//! [`TurnstileSelectors`] for why none of it is a literal in this crate.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::time::Instant;
use zendriver_transport::SessionHandle;

use crate::click::click_at;
use crate::detection::{observe, scroll_widget_into_view};
use crate::error::CloudflareError;
use crate::options::{ClickPolicy, ClickTarget, TurnstileSelectors};

/// Result of a clearance attempt.
#[derive(Debug, Clone)]
pub enum ClearanceOutcome {
    /// Turnstile produced a token — the value held by the first of the
    /// configured [`token_inputs`](TurnstileSelectors::token_inputs) to
    /// match, `[name="cf-turnstile-response"]` by default.
    TokenAcquired(String),
    /// Every challenge marker seen earlier in the run is gone without a
    /// token having shown, and the page has settled. Either a navigation
    /// replaced the document and the new one has been parsed, or the
    /// markers stayed gone for three seconds on the same document, as for a
    /// widget cleared in place. Cloudflare's full-page challenge ends the
    /// first way, on the page it guards: each of its own pages carries a
    /// marker with the default selectors.
    ///
    /// **Not proof the gate was passed.** It says no marker is left, not
    /// that the server let you through: a replacement document whose every
    /// marker is added by script after it is parsed lands here too. Confirm
    /// the page before treating what you scrape next as clean.
    ChallengeGone,
    /// Deadline elapsed without a terminal clearance state. `saw_challenge`
    /// is `true` if any challenge marker (container, hidden input, or live
    /// iframe) was ever observed; `false` if none ever appeared — the caller
    /// likely invoked the bypass on a page that has no Cloudflare gate at all.
    /// Not a fault: a deadline in a bot-management flow is a normal "didn't
    /// finish, retry or give up" terminal.
    TimedOut { saw_challenge: bool },
}

/// Default poll interval for `wait_for_clearance`.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long every marker must stay gone from the *same* document before the
/// run calls the challenge gone: the time a page gets to navigate away after
/// its widget goes, before the run takes it as a widget cleared in place.
///
/// Cloudflare's full-page challenge does not depend on it: every page of
/// that challenge keeps a marker (the script in the default
/// [`container`](TurnstileSelectors::container)) until a navigation
/// replaces it, and the run waits for a replacement without one, however
/// long that takes.
const SETTLE: Duration = Duration::from_secs(3);

/// A caller-supplied click implementation, as stored on the driver.
///
/// The public entry is [`CloudflareBypass::on_click`], which is generic over
/// an ordinary async closure and does the boxing. The session is handed over
/// by value — it is `Arc`-backed and cheap to clone — so the returned future
/// borrows nothing and needs no lifetime of its own.
type ClickHandler = Box<
    dyn Fn(
            SessionHandle,
            ClickTarget,
        ) -> Pin<Box<dyn Future<Output = Result<(), CloudflareError>> + Send>>
        + Send
        + Sync,
>;

/// Drives a Cloudflare Turnstile clearance flow against a single tab's session.
///
/// Constructed via `Tab::cloudflare()`.
pub struct CloudflareBypass<'a> {
    pub(crate) session: &'a SessionHandle,
    pub(crate) poll_interval: Duration,
    pub(crate) selectors: TurnstileSelectors,
    pub(crate) click_policy: ClickPolicy,
    pub(crate) on_click: Option<ClickHandler>,
}

/// Hand-written because `ClickHandler` is a boxed closure and closures are
/// not `Debug`. Reports whether a handler is installed, which is the only
/// thing about it worth printing.
impl std::fmt::Debug for CloudflareBypass<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudflareBypass")
            .field("session", &self.session)
            .field("poll_interval", &self.poll_interval)
            .field("selectors", &self.selectors)
            .field("click_policy", &self.click_policy)
            .field("on_click", &self.on_click.is_some())
            .finish()
    }
}

impl<'a> CloudflareBypass<'a> {
    /// Create a new bypass driver bound to `session` with a default 500ms
    /// poll interval, the default [`TurnstileSelectors`] and the default
    /// [`ClickPolicy`].
    pub fn new(session: &'a SessionHandle) -> Self {
        Self {
            session,
            poll_interval: DEFAULT_POLL_INTERVAL,
            selectors: TurnstileSelectors::default(),
            click_policy: ClickPolicy::default(),
            on_click: None,
        }
    }

    /// Override the default 500ms poll interval used by
    /// [`wait_for_clearance`](Self::wait_for_clearance).
    #[must_use]
    pub fn poll_interval(mut self, dur: Duration) -> Self {
        self.poll_interval = dur;
        self
    }

    /// Override the page markers that identify the widget. Defaults to
    /// [`TurnstileSelectors::default`].
    ///
    /// The same set feeds the presence probe and the poll loop, so
    /// Cloudflare renaming something is one change here rather than a
    /// release of this crate.
    #[must_use]
    pub fn selectors(mut self, selectors: TurnstileSelectors) -> Self {
        self.selectors = selectors;
        self
    }

    /// Override whether, how often and where the widget is clicked. Defaults
    /// to [`ClickPolicy::default`].
    #[must_use]
    pub fn click_policy(mut self, policy: ClickPolicy) -> Self {
        self.click_policy = policy;
        self
    }

    /// Replace the built-in click with `handler`.
    ///
    /// The built-in click is three raw `Input.dispatchMouseEvent` calls at
    /// the point [`ClickPolicy`] chose; `zendriver`'s `Tab::cloudflare`
    /// replaces it with a person's click, made with the tab's mouse. The
    /// click is the volatile half of this
    /// flow — the half that has to change when Cloudflare changes what a
    /// convincing click looks like — so it is handed over whole rather than
    /// grown a knob at a time. A handler might move the pointer along a
    /// human-ish path, drive a solver service, or click through a higher
    /// level of `zendriver` instead.
    ///
    /// The handler is called once per attempt, with the session and the
    /// widget's post-scroll [`ClickTarget`]. It counts against
    /// [`ClickPolicy::max_attempts`] exactly as the built-in click does, and
    /// an `Err` from it fails the whole run rather than being retried.
    ///
    /// It replaces the click, not the decision to click. The driver still
    /// owns what counts as clickable, and only invokes the handler for a
    /// widget that cleared that bar when measured after the scroll, where
    /// the bar is non-zero size and not hidden by `visibility` / `display` /
    /// `opacity`. A handler
    /// installed specifically to reach a hidden or zero-size widget never
    /// runs; that widget needs a different approach entirely.
    ///
    /// ```no_run
    /// # async fn ex(tab: &zendriver_transport::SessionHandle)
    /// #   -> Result<(), zendriver_cloudflare::CloudflareError> {
    /// use std::time::Duration;
    /// use zendriver_cloudflare::CloudflareBypass;
    ///
    /// let outcome = CloudflareBypass::new(tab)
    ///     .on_click(|session, target| async move {
    ///         tracing::info!(x = target.x, y = target.y, "clicking turnstile");
    ///         session
    ///             .call(
    ///                 "Input.dispatchMouseEvent",
    ///                 serde_json::json!({
    ///                     "type": "mouseMoved", "x": target.x, "y": target.y,
    ///                 }),
    ///             )
    ///             .await?;
    ///         Ok(())
    ///     })
    ///     .wait_for_clearance(Duration::from_secs(30))
    ///     .await?;
    /// # let _ = outcome;
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn on_click<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(SessionHandle, ClickTarget) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), CloudflareError>> + Send + 'static,
    {
        self.on_click = Some(Box::new(move |session, target| {
            Box::pin(handler(session, target))
        }));
        self
    }

    /// Poll the page until a Turnstile clearance terminal state is reached or
    /// `timeout` elapses.
    ///
    /// Handles both the **interactive** flow (challenge iframe mounts → we
    /// click it → token appears) and the **invisible** flow (no iframe;
    /// token is populated directly by Cloudflare's loader script). Resolution
    /// rules per tick are documented at module level.
    ///
    /// # Returns
    /// - `Ok(ClearanceOutcome::TokenAcquired(token))` — a configured token
    ///   input picked up a non-empty value, either after we clicked the
    ///   interactive iframe or because the page uses invisible Turnstile.
    /// - `Ok(ClearanceOutcome::ChallengeGone)` — every challenge marker
    ///   observed on an earlier tick is gone, and the page has settled: it
    ///   was replaced by a new, parsed document (how Cloudflare's full-page
    ///   challenge ends), or it stayed without markers for three seconds. Read
    ///   [`ClearanceOutcome::ChallengeGone`] before treating this as proof
    ///   the gate was passed.
    /// - `Ok(ClearanceOutcome::TimedOut { saw_challenge })` — `timeout`
    ///   elapsed without a terminal clearance state. `saw_challenge` is
    ///   `true` if challenge markers were observed (markers present but
    ///   never resolved); `false` if none ever appeared (the caller likely
    ///   invoked the bypass on a page that has no Cloudflare gate). Not a
    ///   fault — a deadline here is a normal "retry or give up" signal.
    ///
    /// # Errors
    /// - [`CloudflareError::Call`] / [`CloudflareError::JsError`] — CDP or
    ///   in-page evaluator failure.
    ///
    /// ```no_run
    /// # async fn ex(tab: &zendriver_transport::SessionHandle)
    /// #   -> Result<(), zendriver_cloudflare::CloudflareError> {
    /// use std::time::Duration;
    /// use zendriver_cloudflare::CloudflareBypass;
    ///
    /// let bypass = CloudflareBypass::new(tab)
    ///     .poll_interval(Duration::from_millis(250));
    /// let outcome = bypass.wait_for_clearance(Duration::from_secs(15)).await?;
    /// println!("{outcome:?}");
    /// # Ok(()) }
    /// ```
    pub async fn wait_for_clearance(
        self,
        timeout: Duration,
    ) -> Result<ClearanceOutcome, CloudflareError> {
        let deadline = Instant::now() + timeout;
        let mut clicks: u32 = 0;
        let mut ticks_since_click: u32 = 0;
        // The document the markers were last seen on (`None` while no marker
        // has been seen), and the time they were first found missing since.
        let mut challenge_document: Option<String> = None;
        let mut markers_gone_since: Option<Instant> = None;
        let mut world = None;

        let mut stall_ticks: u32 = 0;
        let mut warned_stall = false;

        loop {
            let mut clicked = false;
            let mut stalled = false;
            // `None` is a tick on which the page changed under the
            // observation. It says nothing about the challenge, so it decides
            // nothing either.
            if let Some(seen) = observe(self.session, &self.selectors, &mut world).await? {
                if let Some(token) = seen.token {
                    return Ok(ClearanceOutcome::TokenAcquired(token));
                }

                if seen.has_markers() {
                    challenge_document = Some(seen.document.clone());
                    markers_gone_since = None;
                } else if let Some(challenge) = &challenge_document {
                    // Every marker seen earlier is gone. A new document must
                    // be parsed before the run can say what is on it, however
                    // long that takes; the same document only has to stay
                    // this way for `SETTLE`, in case a navigation is coming.
                    let since = *markers_gone_since.get_or_insert_with(Instant::now);
                    let gone = if seen.document == *challenge {
                        since.elapsed() >= SETTLE
                    } else {
                        !seen.loading
                    };
                    if gone {
                        return Ok(ClearanceOutcome::ChallengeGone);
                    }
                }

                // Click cadence: the first clickable tick clicks straight
                // away, every later attempt waits `ClickPolicy::retry_ticks`
                // ticks after the click that landed, and a run spends at most
                // `ClickPolicy::max_attempts` clicks. So at the defaults the
                // second click lands on tick 4 (the click's own tick counts).
                // `max_attempts: 0`
                // never clicks at all, which also keeps the page from being
                // scrolled.
                let may_click = clicks < self.click_policy.max_attempts
                    && (clicks == 0 || ticks_since_click >= self.click_policy.retry_ticks);

                // Raw `Input.dispatchMouseEvent` coordinates are viewport
                // relative, so a below-the-fold widget must be scrolled in and
                // measured before the click can land on it. Measuring only
                // here keeps a tick that will not click from touching the
                // widget at all.
                let mut widget = None;
                if may_click {
                    widget = seen.widget;
                }
                if let Some(node) = widget {
                    let scrolled = scroll_widget_into_view(self.session, node, seen.context);
                    if let Some(bbox) = scrolled.await? {
                        let target = ClickTarget {
                            bbox,
                            x: bbox.x + bbox.width * self.click_policy.x_fraction,
                            y: bbox.y + bbox.height * self.click_policy.y_fraction,
                        };
                        match &self.on_click {
                            Some(handler) => handler(self.session.clone(), target).await?,
                            None => click_at(self.session, target.x, target.y).await?,
                        }
                        clicks += 1;
                        ticks_since_click = 0;
                        clicked = true;
                    }
                }
                stalled = seen.has_markers() && !clicked;
            }

            // A tick that saw a marker and did not land a click is a tick with
            // no progress, the scroll-returned-`None` path included. That path
            // used to skip the counter, so a widget that stayed mounted but
            // stopped being measurable — the run this hint exists to explain —
            // was the one run that could never produce it. A tick without
            // markers is the page settling (or no challenge at all), and a
            // tick without an observation is the page changing: neither is
            // clicks going nowhere.
            //
            // Nor is a run where the caller set `max_attempts: 0`. The hint reads
            // "clicks are going nowhere, check stealth", and a run configured
            // never to click has no clicks to go nowhere: `stall_ticks` only
            // resets on a landed click, so it would fire on every watch-only
            // run and blame stealth for behaviour the caller asked for.
            if clicked {
                stall_ticks = 0;
            } else if stalled && self.click_policy.max_attempts > 0 {
                stall_ticks += 1;
                if stall_ticks == 10 && !warned_stall {
                    tracing::warn!(
                        poll_interval_ms = self.poll_interval.as_millis() as u64,
                        "cloudflare clearance stalled — is BrowserBuilder::stealth enabled?"
                    );
                    // `stall_ticks` resets on a landed click, so `== 10` is
                    // reachable more than once per run; the latch is what
                    // keeps the hint to a single line.
                    warned_stall = true;
                }
            }

            // Only meaningful once something has been clicked — unguarded, it
            // counted ticks before the first click, which the `clicks == 0`
            // arm of `may_click` then had to ignore.
            if clicks > 0 {
                ticks_since_click = ticks_since_click.saturating_add(1);
            }

            if Instant::now() >= deadline {
                return Ok(ClearanceOutcome::TimedOut {
                    saw_challenge: challenge_document.is_some(),
                });
            }

            tokio::select! {
                () = tokio::time::sleep(self.poll_interval) => {}
                () = tokio::time::sleep_until(deadline) => {}
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::Value;

    use super::*;
    use crate::testutil::{CMD_BUDGET, MOUSE, Page, SCROLL, polls, presses, rect, saw, serve};
    use zendriver_transport::CallError;
    use zendriver_transport::testing::{LogCapture, MockConnection};

    /// The token a fake page ends a run with.
    const END_TOKEN: &str = "END";

    /// Bounded `JoinHandle` await for a driver task.
    ///
    /// `wait_for_clearance` polls until a terminal or its own deadline, so a
    /// regression that makes a terminal unreachable leaves the handle
    /// pending forever. Unbounded, the test then hangs instead of failing.
    async fn outcome_of(
        fut: tokio::task::JoinHandle<Result<ClearanceOutcome, CloudflareError>>,
    ) -> Result<ClearanceOutcome, CloudflareError> {
        match tokio::time::timeout(CMD_BUDGET, fut).await {
            Ok(joined) => joined.expect("driver task panicked"),
            Err(_) => panic!("driver never reached a terminal"),
        }
    }

    /// Run `bypass` (configured from a fresh driver) against the fake page
    /// `page(tick)`, with a 1ms poll interval and a 5s deadline unless
    /// `bypass` sets its own. Returns the outcome and every command the
    /// driver sent.
    async fn run(
        bypass: impl FnOnce(CloudflareBypass<'_>) -> CloudflareBypass<'_> + Send + 'static,
        page: impl Fn(usize) -> Page,
    ) -> (
        Result<ClearanceOutcome, CloudflareError>,
        Vec<crate::testutil::Call>,
    ) {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let fut = tokio::spawn(async move {
            let driver = CloudflareBypass::new(&sess).poll_interval(Duration::from_millis(1));
            bypass(driver)
                .wait_for_clearance(Duration::from_secs(5))
                .await
        });
        let calls = serve(&mut mock, page).await;
        let outcome = outcome_of(fut).await;
        conn.shutdown();
        (outcome, calls)
    }

    fn token_of(outcome: Result<ClearanceOutcome, CloudflareError>) -> String {
        match outcome.unwrap() {
            ClearanceOutcome::TokenAcquired(token) => token,
            other => panic!("expected TokenAcquired, got {other:?}"),
        }
    }

    /// A widget rect the default policy and the custom policies below all
    /// map to *different* click points, so no assertion here can be
    /// satisfied by the wrong policy having been applied.
    fn widget_rect() -> Value {
        rect(100.0, 200.0, 80.0, 40.0)
    }

    /// Where the default policy clicks [`widget_rect`] horizontally: 7% in.
    /// A literal rather than a read of `ClickPolicy::default`, so that
    /// changing the default fails these tests instead of moving them along.
    const DEFAULT_X: f64 = 100.0 + 80.0 * 0.07;

    /// The stall hint must fire for the run it is most needed on: a widget
    /// that stays mounted but never measures as a click target, so no click
    /// ever lands.
    #[tokio::test]
    async fn stalled_unmeasurable_widget_emits_the_stealth_hint() {
        /// Stalled ticks to serve before ending the run. The hint fires on
        /// the tenth.
        const STALLED_TICKS: usize = 12;

        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let capture = LogCapture::new();

        // Not spawned: `LogCapture` follows the future it wraps, not a task
        // spawned out of it, so the driver and the fake page share a task.
        let driver = capture.capture(async {
            CloudflareBypass::new(&sess)
                .poll_interval(Duration::from_millis(1))
                .wait_for_clearance(Duration::from_secs(30))
                .await
        });
        let page = |tick| {
            if tick >= STALLED_TICKS {
                Page::challenge().token(END_TOKEN)
            } else {
                Page::challenge().widget_at(Value::Null)
            }
        };
        let (outcome, calls) = tokio::join!(driver, serve(&mut mock, page));

        assert_eq!(token_of(outcome), END_TOKEN);
        assert!(
            !saw(&calls, MOUSE),
            "nothing measurable, so nothing clicked"
        );
        assert_eq!(
            capture.count("clearance stalled"),
            1,
            "a mounted-but-unmeasurable widget is a stalled run, reported once; captured {:?}",
            capture.events()
        );
        conn.shutdown();
    }

    /// The same hint must **not** fire for a caller who set
    /// `max_attempts: 0`. It reads "clearance stalled — is stealth
    /// enabled?", and `stall_ticks` only resets on a landed click, so a run
    /// configured never to click would trip it every time and send the
    /// caller off to change an unrelated browser setting.
    #[tokio::test]
    async fn a_watch_only_run_is_never_blamed_on_stealth() {
        /// Past the tenth tick, which is where the hint fires.
        const MOUNTED_TICKS: usize = 12;

        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let capture = LogCapture::new();

        let driver = capture.capture(async {
            CloudflareBypass::new(&sess)
                .poll_interval(Duration::from_millis(1))
                .click_policy(ClickPolicy {
                    max_attempts: 0,
                    ..ClickPolicy::default()
                })
                .wait_for_clearance(Duration::from_secs(30))
                .await
        });
        let page = |tick| {
            if tick >= MOUNTED_TICKS {
                Page::challenge().token(END_TOKEN)
            } else {
                Page::challenge().widget_at(widget_rect())
            }
        };
        let (outcome, calls) = tokio::join!(driver, serve(&mut mock, page));

        // The token only lands after MOUNTED_TICKS polls, so reaching it
        // proves the run went past the tick the hint fires on.
        assert_eq!(token_of(outcome), END_TOKEN);
        assert!(!saw(&calls, MOUSE), "the policy said do not click");
        assert!(
            !capture.contains("clearance stalled"),
            "a run told not to click is not a stalled run; captured {:?}",
            capture.events()
        );
        conn.shutdown();
    }

    /// Interactive happy path: the widget is scrolled into view, measured
    /// there, and clicked at (7% × width, 50% × height) of that rect; the
    /// next poll sees the token.
    #[tokio::test]
    async fn scrolls_the_widget_into_view_then_clicks_its_post_scroll_rect() {
        let (outcome, calls) = run(
            |b| b,
            |tick| match tick {
                0 => Page::challenge().widget_at(rect(100.0, 300.0, 60.0, 40.0)),
                _ => Page::challenge().token("TOKEN_XYZ"),
            },
        )
        .await;

        assert_eq!(token_of(outcome), "TOKEN_XYZ");
        // (100 + 60 * 0.07, 300 + 40 * 0.5)
        assert_eq!(presses(&calls), vec![(0, (100.0 + 60.0 * 0.07, 320.0))]);
        let scroll = calls.iter().position(|c| c.label == SCROLL);
        let first_mouse = calls.iter().position(|c| c.label.starts_with(MOUSE));
        assert!(
            scroll.is_some() && scroll < first_mouse,
            "a click must be preceded by a scroll-into-view; observed {calls:?}"
        );
        let poll = calls
            .iter()
            .find(|c| c.label == "Runtime.evaluate")
            .unwrap();
        assert!(
            poll.params["expression"]
                .as_str()
                .unwrap()
                .contains("cf-turnstile-response"),
            "the poll evaluator should read the cf-turnstile-response input"
        );
    }

    /// A widget that mounts *late* and *below the fold* still gets clicked,
    /// at its post-scroll position. Every other click test hands the driver
    /// a widget on tick 0, so all of them pass against a driver that only
    /// ever considers clicking on its first tick; this one does not.
    #[tokio::test]
    async fn clicks_a_widget_that_mounts_late_and_below_the_fold() {
        /// Ticks that show markers but no widget yet.
        const EMPTY_TICKS: usize = 3;

        let (outcome, calls) = run(
            |b| b,
            |tick| match tick {
                t if t < EMPTY_TICKS => Page::challenge(),
                EMPTY_TICKS => Page::challenge().widget_at(widget_rect()),
                _ => Page::challenge().token(END_TOKEN),
            },
        )
        .await;

        assert_eq!(token_of(outcome), END_TOKEN);
        // (100 + 80 * 0.07, 200 + 40 * 0.5) — the post-scroll rect.
        assert_eq!(presses(&calls), vec![(EMPTY_TICKS, (DEFAULT_X, 220.0))]);
    }

    /// With site isolation on, the cross-site widget is not in the page's
    /// frame tree at all; it is an `"iframe"` target of its own. It must be
    /// found, and clicked, all the same.
    #[tokio::test]
    async fn clicks_a_widget_running_in_a_process_of_its_own() {
        let (outcome, calls) = run(
            |b| b,
            |tick| match tick {
                0 => Page::challenge().widget_at(widget_rect()).out_of_process(),
                _ => Page::challenge().token(END_TOKEN),
            },
        )
        .await;

        assert_eq!(token_of(outcome), END_TOKEN);
        assert_eq!(presses(&calls), vec![(0, (DEFAULT_X, 220.0))]);
    }

    /// Invisible-Turnstile path: no widget ever mounts, the token is
    /// populated directly on the first poll → TokenAcquired without any
    /// click.
    #[tokio::test]
    async fn returns_the_token_without_a_click_for_invisible_turnstile() {
        let (outcome, calls) = run(|b| b, |_| Page::challenge().token("INVISIBLE")).await;

        assert_eq!(token_of(outcome), "INVISIBLE");
        assert!(!saw(&calls, MOUSE), "the invisible path never clicks");
        assert!(
            !saw(&calls, "Target.getTargets"),
            "a token in hand ends the tick before the widget lookup"
        );
    }

    /// Markers present but no widget yet (invisible Turnstile still
    /// computing): the loop keeps waiting for the token.
    #[tokio::test]
    async fn keeps_waiting_while_markers_are_present() {
        let (outcome, calls) = run(
            |b| b,
            |tick| match tick {
                0 | 1 => Page::challenge(),
                _ => Page::challenge().token("LATE"),
            },
        )
        .await;

        assert_eq!(token_of(outcome), "LATE");
        assert_eq!(polls(&calls), 3);
    }

    /// Cloudflare's full-page challenge, as watched live: after the click
    /// its widget goes, then its token input, and only a second or two
    /// later does a navigation replace the page. The run must end on the
    /// replacement — parsed — not on the challenge page's last moments.
    #[tokio::test]
    async fn challenge_gone_waits_for_the_page_that_replaces_the_challenge() {
        let (outcome, calls) = run(
            |b| b,
            |tick| match tick {
                0 => Page::challenge().widget_at(widget_rect()),
                // The widget is removed; the token input stays a moment.
                1 => Page::challenge(),
                // Nothing left, but this is still the challenge's document.
                2 | 3 => Page::challenge().no_light_markers(),
                // The guarded page has committed and is being parsed.
                4 => Page::challenge()
                    .no_light_markers()
                    .document("D2")
                    .loading(),
                _ => Page::challenge().no_light_markers().document("D2"),
            },
        )
        .await;

        assert!(
            matches!(outcome.unwrap(), ClearanceOutcome::ChallengeGone),
            "expected ChallengeGone"
        );
        assert_eq!(
            polls(&calls),
            6,
            "the run must end on the first tick showing the parsed replacement"
        );
    }

    /// A challenge that clears in place never brings a new document. Once its
    /// markers have stayed gone for `SETTLE`, the run calls it gone; not a
    /// moment before, since a navigation could still be on its way.
    #[tokio::test(start_paused = true)]
    async fn challenge_gone_in_place_waits_out_the_settle_period() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let fut = tokio::spawn(async move {
            let started = Instant::now();
            let outcome = CloudflareBypass::new(&sess)
                .poll_interval(Duration::from_millis(250))
                .wait_for_clearance(Duration::from_secs(60))
                .await;
            (outcome, started.elapsed())
        });
        let page = |tick| match tick {
            0 => Page::challenge(),
            _ => Page::challenge().no_light_markers(),
        };
        serve(&mut mock, page).await;
        let (outcome, elapsed) = fut.await.unwrap();

        assert!(
            matches!(outcome.unwrap(), ClearanceOutcome::ChallengeGone),
            "expected ChallengeGone"
        );
        assert!(
            elapsed >= SETTLE && elapsed < SETTLE + Duration::from_secs(1),
            "ChallengeGone came {elapsed:?} into the run"
        );
        conn.shutdown();
    }

    /// A replacement document still being parsed is waited for, however long
    /// it streams: `SETTLE` bounds only the wait on the *same* document,
    /// and returning on a half-parsed page would hand the caller a page the
    /// run has not read.
    #[tokio::test(start_paused = true)]
    async fn a_replacement_still_loading_is_waited_for_past_the_settle_period() {
        /// Ticks of 250ms the replacement streams for: past `SETTLE`.
        const LOADING_TICKS: usize = 20;

        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let fut = tokio::spawn(async move {
            CloudflareBypass::new(&sess)
                .poll_interval(Duration::from_millis(250))
                .wait_for_clearance(Duration::from_secs(60))
                .await
        });
        let page = |tick| match tick {
            0 => Page::challenge(),
            t if t <= LOADING_TICKS => Page::challenge()
                .no_light_markers()
                .document("D2")
                .loading(),
            _ => Page::challenge().no_light_markers().document("D2"),
        };
        let calls = serve(&mut mock, page).await;

        assert!(matches!(
            fut.await.unwrap().unwrap(),
            ClearanceOutcome::ChallengeGone
        ));
        assert_eq!(
            polls(&calls),
            LOADING_TICKS + 2,
            "returned before the parse ended"
        );
        conn.shutdown();
    }

    /// Waiting for the page to settle is not a stalled click, so it must not
    /// tell the caller to go check stealth.
    #[tokio::test(start_paused = true)]
    async fn settling_is_never_blamed_on_stealth() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let capture = LogCapture::new();
        let driver = capture.capture(async {
            CloudflareBypass::new(&sess)
                .poll_interval(Duration::from_millis(100))
                .wait_for_clearance(Duration::from_secs(60))
                .await
        });
        let page = |tick| match tick {
            0 => Page::challenge(),
            _ => Page::challenge().no_light_markers(),
        };
        let (outcome, calls) = tokio::join!(driver, serve(&mut mock, page));

        assert!(matches!(outcome.unwrap(), ClearanceOutcome::ChallengeGone));
        assert!(
            polls(&calls) > 20,
            "the settle period spans more than ten ticks"
        );
        assert!(
            !capture.contains("clearance stalled"),
            "captured {:?}",
            capture.events()
        );
        conn.shutdown();
    }

    /// The widget vanishing after a click is not clearance while its token
    /// input is still on the page: the full-page challenge removes the widget
    /// first, then the input, and navigates last.
    #[tokio::test]
    async fn a_clicked_widget_vanishing_is_not_clearance_while_markers_remain() {
        let (outcome, calls) = run(
            |b| b,
            |tick| match tick {
                0 => Page::challenge().widget_at(widget_rect()),
                1..=3 => Page::challenge(),
                _ => Page::challenge().token(END_TOKEN),
            },
        )
        .await;

        assert_eq!(token_of(outcome), END_TOKEN);
        assert_eq!(presses(&calls).len(), 1);
    }

    /// The page navigating while it is watched makes Chrome answer with an
    /// error ("Cannot find default execution context"). That is a tick with
    /// nothing to report, not a failed run.
    #[tokio::test]
    async fn an_error_answer_mid_navigation_skips_the_tick() {
        let (outcome, calls) = run(
            |b| b,
            |tick| match tick {
                0 => Page::challenge(),
                1 | 2 => Page::challenge().between_documents(),
                _ => Page::challenge().token(END_TOKEN),
            },
        )
        .await;

        assert_eq!(token_of(outcome), END_TOKEN);
        assert_eq!(polls(&calls), 4);
    }

    /// A page with no Cloudflare gate at all must never be reported as
    /// ChallengeGone: with no markers ever observed, the run ends in
    /// `TimedOut { saw_challenge: false }`.
    #[tokio::test]
    async fn times_out_when_no_markers_are_ever_seen() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let fut = tokio::spawn(async move {
            CloudflareBypass::new(&sess)
                .poll_interval(Duration::from_millis(1))
                .wait_for_clearance(Duration::from_millis(40))
                .await
        });
        let calls = serve(&mut mock, |_| Page::challenge().no_light_markers()).await;

        match outcome_of(fut).await.unwrap() {
            ClearanceOutcome::TimedOut { saw_challenge } => assert!(!saw_challenge),
            other => panic!("expected TimedOut, got {other:?}"),
        }
        assert!(polls(&calls) > 1, "the run polled until its deadline");
        conn.shutdown();
    }

    /// A widget that stays mounted and unanswered gets clicked again after
    /// [`ClickPolicy::retry_ticks`] ticks — but never more than
    /// [`ClickPolicy::max_attempts`] times in one run.
    ///
    /// The assertion is the *tick indices*, not the count: a count pins only
    /// the cap. Both numbers are literals rather than reads of
    /// `ClickPolicy::default`, since an expectation computed from the value
    /// the driver reads is satisfied by any change to that value;
    /// `options::tests` is the one place that holds the defaults to 3 and 4.
    #[tokio::test]
    async fn retries_the_click_at_the_default_cadence() {
        /// Ticks the widget stays mounted for. Comfortably past the third
        /// click, so a fourth would have room to show up and fail this.
        const MOUNTED_TICKS: usize = 14;

        let (outcome, calls) = run(
            |b| b,
            |tick| {
                if tick >= MOUNTED_TICKS {
                    Page::challenge().token(END_TOKEN)
                } else {
                    Page::challenge().widget_at(widget_rect())
                }
            },
        )
        .await;

        assert_eq!(token_of(outcome), END_TOKEN);
        let ticks: Vec<usize> = presses(&calls).iter().map(|(tick, _)| *tick).collect();
        assert_eq!(
            ticks,
            vec![0, 4, 8],
            "three clicks, four ticks apart, then nothing for the six ticks after"
        );
    }

    /// `max_attempts: 0` is the caller saying "watch, do not touch". The
    /// driver must then never dispatch a mouse event, and never scroll
    /// either: scrolling a page the caller asked us not to touch is itself a
    /// change they did not ask for.
    ///
    /// Deadline-terminated, because this is also the crate's coverage of
    /// `TimedOut { saw_challenge: true }` — which can only be reached by
    /// ticks that ran and saw markers, so the absence assertions cannot pass
    /// by the run having done nothing at all.
    #[tokio::test]
    async fn click_policy_zero_attempts_never_dispatches_a_click() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let fut = tokio::spawn(async move {
            CloudflareBypass::new(&sess)
                .poll_interval(Duration::from_millis(1))
                .click_policy(ClickPolicy {
                    max_attempts: 0,
                    ..ClickPolicy::default()
                })
                .wait_for_clearance(Duration::from_millis(60))
                .await
        });
        let calls = serve(&mut mock, |_| Page::challenge().widget_at(widget_rect())).await;

        assert!(!saw(&calls, MOUSE), "max_attempts: 0 must not click");
        assert!(!saw(&calls, SCROLL), "nor scroll the page");
        assert!(!saw(&calls, "DOM.resolveNode"), "nor touch the widget");
        match outcome_of(fut).await.unwrap() {
            ClearanceOutcome::TimedOut { saw_challenge } => assert!(saw_challenge),
            other => panic!("expected TimedOut, got {other:?}"),
        }
        conn.shutdown();
    }

    /// The attempt cap and the retry cadence are both the caller's to set,
    /// and both move here at once: five attempts where the built-in cap is
    /// three, one tick apart where the built-in cadence is four.
    #[tokio::test]
    async fn click_policy_sets_the_attempt_cap_and_the_retry_cadence() {
        /// Four ticks past the last expected click, so a sixth attempt has
        /// somewhere to appear.
        const MOUNTED_TICKS: usize = 9;

        let (outcome, calls) = run(
            |b| {
                b.click_policy(ClickPolicy {
                    max_attempts: 5,
                    retry_ticks: 1,
                    ..ClickPolicy::default()
                })
            },
            |tick| {
                if tick >= MOUNTED_TICKS {
                    Page::challenge().token(END_TOKEN)
                } else {
                    Page::challenge().widget_at(widget_rect())
                }
            },
        )
        .await;

        assert_eq!(token_of(outcome), END_TOKEN);
        let ticks: Vec<usize> = presses(&calls).iter().map(|(tick, _)| *tick).collect();
        assert_eq!(ticks, vec![0, 1, 2, 3, 4]);
    }

    /// Where inside the widget to click is a volatile detail of Cloudflare's
    /// own layout, so it is caller data.
    #[tokio::test]
    async fn click_policy_moves_the_click_point() {
        let (outcome, calls) = run(
            |b| {
                b.click_policy(ClickPolicy {
                    x_fraction: 0.5,
                    y_fraction: 0.25,
                    ..ClickPolicy::default()
                })
            },
            |tick| match tick {
                0 => Page::challenge().widget_at(widget_rect()),
                _ => Page::challenge().token(END_TOKEN),
            },
        )
        .await;

        assert_eq!(token_of(outcome), END_TOKEN);
        // 100 + 80 * 0.5 = 140, 200 + 40 * 0.25 = 210. The built-in policy
        // would give (105.6, 220).
        assert_eq!(presses(&calls), vec![(0, (140.0, 210.0))]);
    }

    /// `on_click` hands the click to the caller wholesale: the built-in raw
    /// mouse dispatch must not also run, so the decisive assertion is the
    /// *absence* of `Input.dispatchMouseEvent`.
    #[tokio::test]
    async fn on_click_replaces_the_built_in_mouse_dispatch() {
        let seen: Arc<Mutex<Vec<ClickTarget>>> = Arc::new(Mutex::new(Vec::new()));
        let handler_seen = Arc::clone(&seen);
        let (outcome, calls) = run(
            move |b| {
                b.on_click(move |_session, target| {
                    let seen = Arc::clone(&handler_seen);
                    async move {
                        seen.lock().expect("poisoned").push(target);
                        Ok(())
                    }
                })
            },
            |tick| match tick {
                0 => Page::challenge().widget_at(widget_rect()),
                _ => Page::challenge().token(END_TOKEN),
            },
        )
        .await;

        assert_eq!(token_of(outcome), END_TOKEN);
        assert!(!saw(&calls, MOUSE), "on_click owns the click");
        // The handler is handed the post-scroll rect and the point the
        // policy chose inside it.
        let targets = seen.lock().expect("poisoned").clone();
        assert_eq!(targets.len(), 1, "on_click runs once per attempt");
        assert_eq!(targets[0].bbox.y, 200.0);
        assert_eq!((targets[0].x, targets[0].y), (DEFAULT_X, 220.0));
    }

    /// A failing caller click is a failing run — swallowing it would leave
    /// the loop burning its budget on a click that never happens.
    #[tokio::test]
    async fn on_click_error_aborts_the_run() {
        let (outcome, _) = run(
            |b| {
                b.on_click(|_session, _target| async {
                    Err(CloudflareError::JsError("solver exploded".into()))
                })
            },
            |_| Page::challenge().widget_at(widget_rect()),
        )
        .await;

        let err = outcome.expect_err("handler error must propagate");
        assert!(err.to_string().contains("solver exploded"), "got {err}");
    }

    /// The markers are caller data all the way down: a custom marker must
    /// reach the evaluator and the frame lookup the loop actually runs. The
    /// fake page's widget sits at the *default* host, so a driver still
    /// matching the default would find it and click it.
    #[tokio::test]
    async fn configured_markers_reach_the_poll_and_the_frame_lookup() {
        let (outcome, calls) = run(
            |b| {
                b.selectors(TurnstileSelectors {
                    iframe_src_contains: "gate.alien.invalid".into(),
                    container: ".alien-widget".into(),
                    token_inputs: vec!["[name=\"alien-token\"]".into()],
                })
            },
            |tick| match tick {
                0 | 1 => Page::challenge().widget_at(widget_rect()),
                _ => Page::challenge().token(END_TOKEN),
            },
        )
        .await;

        assert_eq!(token_of(outcome), END_TOKEN);
        let poll = calls
            .iter()
            .find(|c| c.label == "Runtime.evaluate")
            .unwrap();
        let poll = poll.params["expression"].as_str().unwrap();
        assert!(poll.contains(".alien-widget") && poll.contains("alien-token"));
        assert!(
            !poll.contains("cf-turnstile"),
            "default markers still baked in"
        );
        assert!(
            !saw(&calls, "DOM.getFrameOwner"),
            "a frame at the default host matched a custom marker"
        );
    }

    /// The poll and the measurement run in an isolated world, out of the
    /// page's sight (the fake page asserts which world each call names), and
    /// a world is made once per document: again after a navigation, not on
    /// every tick.
    #[tokio::test]
    async fn one_isolated_world_serves_each_document() {
        let (outcome, calls) = run(
            |b| b,
            |tick| match tick {
                0 => Page::challenge(),
                1 => Page::challenge().widget_at(widget_rect()),
                2 | 3 => Page::challenge().document("D2"),
                _ => Page::challenge().document("D2").token(END_TOKEN),
            },
        )
        .await;

        assert_eq!(token_of(outcome), END_TOKEN);
        let worlds = calls
            .iter()
            .filter(|c| c.label == "Page.createIsolatedWorld")
            .count();
        assert_eq!(worlds, 2, "one world for D1, one for D2");
        assert_eq!(presses(&calls).len(), 1);
    }

    /// Only a server error reads as the page changing under the call. Any
    /// other error answer — here, a Chrome without the method — fails the
    /// run, rather than leaving it to time out as if the page had no gate.
    #[tokio::test]
    async fn an_error_answer_no_navigation_explains_fails_the_run() {
        let (outcome, _) = run(|b| b, |_| Page::challenge().refusing_the_poll()).await;

        let err = outcome.expect_err("the refusal must surface");
        assert!(
            matches!(err, CloudflareError::Call(CallError::Rpc(-32601, ..))),
            "got {err}"
        );
    }
}
