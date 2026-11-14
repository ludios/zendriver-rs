//! Cloudflare Turnstile bypass tool — `browser_solve_turnstile`. Gated
//! behind the `cloudflare` feature.
//!
//! ## Outcome mapping
//!
//! The lib's [`CloudflareBypass::wait_for_clearance`] models its terminal
//! state as a `Result<ClearanceOutcome, CloudflareError>` where:
//!
//! - `Ok(ClearanceOutcome::TokenAcquired(t))` — a configured token input
//!   (`cf-turnstile-response` by default) picked up a token.
//! - `Ok(ClearanceOutcome::ChallengeGone)` — every challenge marker is gone
//!   without a token, from a page that has settled (for Cloudflare's
//!   full-page challenge, the page it navigates to once passed).
//! - `Ok(ClearanceOutcome::TimedOut { saw_challenge })` — the per-call
//!   deadline elapsed (whether or not a challenge was ever seen).
//!
//! Agents typically want all three lumped into a single discriminated
//! union of *expected* outcomes — a timeout in turnstile flow is a normal
//! "didn't finish, try again or give up" signal, not a server error. So
//! the MCP layer collapses `TimedOut` into a third [`Outcome::Timeout`]
//! variant, and keeps every `CloudflareError` (network failure, JS error)
//! as a real MCP error.
//!
//! [`CloudflareBypass::wait_for_clearance`]: zendriver_cloudflare::CloudflareBypass::wait_for_clearance

#![cfg(feature = "cloudflare")]

use std::sync::Arc;
use std::time::Duration;

use rmcp::ErrorData;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use zendriver::{
    ClearanceOutcome, ClickPolicy, CloudflareBypass, TurnstileSelectors, ZendriverError,
};

use crate::errors::{McpServerError, map_error};
use crate::state::SessionState;
use crate::tools::common::current_tab;

/// Input for `browser_solve_turnstile`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SolveInput {
    /// Maximum total wait for a terminal outcome, in milliseconds. Default
    /// 30_000 (30s) — the lib's documented sane default for a real Turnstile
    /// flow. Lower values can be used in tests or when the agent wants to
    /// fail fast and fall back to another strategy.
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// Override the bypass driver's internal poll cadence, in milliseconds.
    /// Defaults to the lib's own default (500ms). Lowering speeds up
    /// detection at the cost of more CDP `Runtime.evaluate` round-trips;
    /// raising is a safe knob for slow / sandboxed environments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_ms: Option<u64>,
    /// Override the page markers that identify the Turnstile widget. Omit to
    /// use the lib's defaults, which track Cloudflare's current markup.
    /// Cloudflare owns this markup and changes it without notice, so an
    /// agent that finds the defaults no longer matching a page can point the
    /// solver at the new markers here instead of waiting on a release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selectors: Option<SelectorsInput>,
    /// Override whether, how often and where the widget is clicked. Omit for
    /// the lib's defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub click_policy: Option<ClickPolicyInput>,
}

/// Page markers identifying the Turnstile widget — the request-side mirror
/// of the lib's `TurnstileSelectors`.
///
/// Every field is optional and falls back to the lib default *individually*,
/// so overriding one marker does not silently blank the others.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SelectorsInput {
    /// Substring an `<iframe>`'s `src` must contain for it to be the
    /// challenge widget. Defaults to `"challenges.cloudflare.com"`. An empty
    /// string disables the iframe signal rather than matching every iframe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iframe_src_contains: Option<String>,
    /// CSS selector for elements that mark a challenge as on the page, used
    /// as a presence signal: the container a site author puts Turnstile in,
    /// and the script each page of Cloudflare's full-page challenge loads.
    /// Defaults to
    /// `.cf-turnstile, .turnstile, [data-sitekey], script[src*="/cdn-cgi/challenge-platform/"][src*="/orchestrate/"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// CSS selectors for the hidden input carrying the clearance token, in
    /// preference order. Defaults to
    /// `["[name=\"cf-turnstile-response\"]", "[name=\"cf_challenge_response\"]"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_inputs: Option<Vec<String>>,
}

impl SelectorsInput {
    /// Fold into the lib type, field by field, over the lib's defaults.
    fn into_lib(self) -> TurnstileSelectors {
        let d = TurnstileSelectors::default();
        TurnstileSelectors {
            iframe_src_contains: self.iframe_src_contains.unwrap_or(d.iframe_src_contains),
            container: self.container.unwrap_or(d.container),
            token_inputs: self.token_inputs.unwrap_or(d.token_inputs),
        }
    }
}

/// Click behaviour — the request-side mirror of the lib's `ClickPolicy`.
/// Same per-field fallback rule as [`SelectorsInput`].
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClickPolicyInput {
    /// How many clicks one solve may spend on the widget. `0` watches
    /// without ever clicking, for pages whose own script drives the widget.
    /// Defaults to `3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    /// Poll ticks to wait after a click before spending another attempt.
    /// Defaults to `4`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_ticks: Option<u32>,
    /// Horizontal click point inside the widget's box, as a fraction of its
    /// width. Defaults to `0.07`, the middle of the Turnstile checkbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_fraction: Option<f64>,
    /// Vertical click point inside the widget's box, as a fraction of its
    /// height. Defaults to `0.5`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y_fraction: Option<f64>,
}

impl ClickPolicyInput {
    /// Fold into the lib type, field by field, over the lib's defaults.
    fn into_lib(self) -> ClickPolicy {
        let d = ClickPolicy::default();
        ClickPolicy {
            max_attempts: self.max_attempts.unwrap_or(d.max_attempts),
            retry_ticks: self.retry_ticks.unwrap_or(d.retry_ticks),
            x_fraction: self.x_fraction.unwrap_or(d.x_fraction),
            y_fraction: self.y_fraction.unwrap_or(d.y_fraction),
        }
    }
}

fn default_timeout() -> u64 {
    30_000
}

/// Apply a request's overrides to a bypass driver, leaving anything the
/// request omitted at the lib's own default.
///
/// Split out of [`solve_turnstile`] purely so it can be tested: the tool
/// needs a live tab to get a driver at all, this needs only a session, and a
/// `MockConnection` supplies one. Without the split, the request fields
/// converting correctly and the request fields ever being *read* were two
/// different claims and only the first had a test.
fn configure<'a>(mut bypass: CloudflareBypass<'a>, input: SolveInput) -> CloudflareBypass<'a> {
    if let Some(p) = input.poll_interval_ms {
        bypass = bypass.poll_interval(Duration::from_millis(p));
    }
    if let Some(selectors) = input.selectors {
        bypass = bypass.selectors(selectors.into_lib());
    }
    if let Some(policy) = input.click_policy {
        bypass = bypass.click_policy(policy.into_lib());
    }
    bypass
}

/// Terminal outcome of a turnstile bypass attempt.
///
/// `Solved` and `ChallengeGone` mirror the lib's `ClearanceOutcome`
/// variants. `Timeout` mirrors the lib's `ClearanceOutcome::TimedOut` — a
/// deadline in a turnstile flow is a normal outcome, not an error —
/// surfacing it on the success channel so agents can branch on `outcome`
/// without try/catch around a timeout.
#[derive(Debug, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Turnstile produced a token — the value held by the first matching
    /// `selectors.token_inputs` entry, `cf-turnstile-response` by default.
    /// The token is available in [`SolveOutput::token`].
    Solved,
    /// Every challenge marker seen earlier is gone without a token, and the
    /// page has settled: a navigation replaced it with a parsed document
    /// (how Cloudflare's full-page challenge ends, on the page it guards),
    /// or it stayed without markers for three seconds. `token` will be
    /// `None`.
    ///
    /// Not proof the gate was passed: a replacement page whose every marker
    /// is added by script after it is parsed lands here too, so verify the
    /// page before treating what you read next as post-challenge content.
    ChallengeGone,
    /// `timeout_ms` elapsed without reaching either success state. `token`
    /// will be `None`. Not a hard error — agents can retry or give up.
    Timeout,
}

/// Output of `browser_solve_turnstile`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct SolveOutput {
    /// Which terminal state the bypass reached.
    pub outcome: Outcome,
    /// Turnstile response token. Populated only when `outcome == Solved`;
    /// `None` for `ChallengeGone` and `Timeout`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// Drive the Turnstile clearance flow on the current tab, returning the
/// terminal outcome (`Solved`, `ChallengeGone`, or `Timeout`) within
/// `timeout_ms`.
///
/// See module-level docs for outcome semantics.
pub async fn solve_turnstile(
    state: Arc<Mutex<SessionState>>,
    input: SolveInput,
) -> Result<SolveOutput, ErrorData> {
    let s = state.lock().await;
    let tab = current_tab(&s).await?;
    // Build the bypass driver. `tab.cloudflare()` borrows `tab` for the
    // bypass's `'_` lifetime, but `wait_for_clearance` consumes `self`, so
    // the bypass lives only for the single await we're about to do.
    let timeout = Duration::from_millis(input.timeout_ms);
    match configure(tab.cloudflare(), input)
        .wait_for_clearance(timeout)
        .await
    {
        Ok(ClearanceOutcome::TokenAcquired(t)) => Ok(SolveOutput {
            outcome: Outcome::Solved,
            token: Some(t),
        }),
        Ok(ClearanceOutcome::ChallengeGone) => Ok(SolveOutput {
            outcome: Outcome::ChallengeGone,
            token: None,
        }),
        // TimedOut is a lib-side success terminal; collapse it into the
        // success-channel `Outcome::Timeout` — see module docs. The old
        // `NoChallenge` error used to fall into the `Err` arm below; it is
        // now folded into this outcome (with `saw_challenge: false`).
        Ok(ClearanceOutcome::TimedOut { .. }) => Ok(SolveOutput {
            outcome: Outcome::Timeout,
            token: None,
        }),
        // Everything else (CDP call failed, JS error) is a real error the
        // agent should surface. Route through the lib's
        // `From<CloudflareError> for ZendriverError` so the existing
        // `map_error` knows how to format it.
        Err(other) => Err(map_error(McpServerError::from(ZendriverError::from(other)))),
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    //! No-browser unit coverage.
    //!
    //! The bypass flow itself needs a live Chrome + a Cloudflare Turnstile
    //! page — that path is exercised in the integration test gated behind
    //! `integration-tests + cloudflare`. Here we cover the only branch
    //! reachable without a browser: calling the tool with no browser open
    //! surfaces `BrowserNotOpen`.

    use super::*;

    #[tokio::test]
    async fn solve_with_no_browser_errors() {
        let state = Arc::new(Mutex::new(SessionState::new()));
        let err = solve_turnstile(
            state,
            SolveInput {
                timeout_ms: 100,
                poll_interval_ms: None,
                selectors: None,
                click_policy: None,
            },
        )
        .await
        .expect_err("expected BrowserNotOpen");
        assert!(err.message.contains("Browser not open"));
    }

    /// The tool's promise is that an agent can follow Cloudflare's markup
    /// when it moves. That needs two things to be true, and only one of them
    /// had a test: that the request types fold onto the lib types correctly
    /// (below), and that the fold is ever actually *called*. Deleting both
    /// wiring branches from the tool left this module green.
    ///
    /// So drive a real bypass over a mock CDP connection and read the wire.
    /// The fake page puts the widget's frame at the request's host, so the
    /// driver finds the widget only if the request's marker reached the
    /// frame lookup; and the click must land where the request's fractions
    /// put it — 7% / 50% of the same box would be (105.6, 220), so neither
    /// coordinate can be produced by a driver that quietly kept its
    /// defaults.
    #[tokio::test]
    async fn request_overrides_reach_the_driver() {
        use serde_json::{Value, json};
        use zendriver::{CloudflareBypass, SessionHandle};
        use zendriver_transport::testing::MockConnection;

        /// The fake page's answer to each command the driver sends: a
        /// challenge page whose widget, an 80×40 box at (100, 200), is held
        /// by a frame at the request's host. Its token shows from the
        /// second poll on.
        fn answer(method: &str, polls: usize) -> Value {
            let widget = json!({ "x": 100.0, "y": 200.0, "width": 80.0, "height": 40.0 });
            match method {
                "Runtime.evaluate" => {
                    let token = if polls > 1 { json!("T") } else { Value::Null };
                    json!({ "result": { "type": "object", "value": {
                        "token": token, "markers": true, "loading": false,
                    } } })
                }
                "Page.getFrameTree" => json!({ "frameTree": {
                    "frame": { "id": "MAIN", "loaderId": "D1" },
                    "childFrames": [{ "frame": { "id": "W", "url": "https://gate.alien.invalid/w" } }],
                } }),
                "Page.createIsolatedWorld" => json!({ "executionContextId": 1 }),
                "Target.getTargets" => json!({ "targetInfos": [] }),
                "DOM.getFrameOwner" => json!({ "backendNodeId": 7 }),
                "DOM.resolveNode" => json!({ "object": { "objectId": "O" } }),
                "Runtime.callFunctionOn" => {
                    json!({ "result": { "type": "object", "value": widget } })
                }
                _ => json!({}),
            }
        }

        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");

        let input = SolveInput {
            timeout_ms: 5_000,
            poll_interval_ms: Some(1),
            selectors: Some(SelectorsInput {
                iframe_src_contains: Some("gate.alien.invalid".into()),
                ..SelectorsInput::default()
            }),
            click_policy: Some(ClickPolicyInput {
                x_fraction: Some(0.5),
                y_fraction: Some(0.25),
                ..ClickPolicyInput::default()
            }),
        };

        // Borrowed, not spawned: the bypass borrows the session, so its
        // future is not `'static`. Driver and mock server share a task.
        let driver = async {
            configure(CloudflareBypass::new(&sess), input)
                .wait_for_clearance(Duration::from_millis(5_000))
                .await
        };

        let serving = async {
            let mut polls = 0;
            let mut point = None;
            while let Some((method, id)) = mock.recv_cmd_timeout(Duration::from_millis(300)).await {
                let params = mock.last_sent()["params"].clone();
                if method == "Runtime.evaluate" {
                    polls += 1;
                }
                if params["type"] == "mousePressed" {
                    point = Some((params["x"].as_f64().unwrap(), params["y"].as_f64().unwrap()));
                }
                mock.reply(id, answer(&method, polls)).await;
            }
            point
        };

        let (outcome, point) = tokio::join!(driver, serving);
        assert_eq!(
            point,
            Some((140.0, 210.0)),
            "the click missed the widget at the request's host, or ignored the request's fractions"
        );
        assert!(matches!(
            outcome.unwrap(),
            zendriver::ClearanceOutcome::TokenAcquired(_)
        ));
        conn.shutdown();
    }

    /// An omitted field means "keep the lib default", not "zero". Folding
    /// with `unwrap_or_default()` instead would turn an agent overriding one
    /// marker into an agent that silently blanked the other two, and an
    /// agent tuning `retry_ticks` into one that disabled clicking outright.
    #[test]
    fn omitted_fields_fall_back_to_the_lib_defaults_one_by_one() {
        let selectors = SelectorsInput {
            iframe_src_contains: Some("gate.example.invalid".into()),
            ..SelectorsInput::default()
        }
        .into_lib();
        let d = TurnstileSelectors::default();
        assert_eq!(selectors.iframe_src_contains, "gate.example.invalid");
        assert_eq!(selectors.container, d.container);
        assert_eq!(selectors.token_inputs, d.token_inputs);

        let policy = ClickPolicyInput {
            retry_ticks: Some(1),
            ..ClickPolicyInput::default()
        }
        .into_lib();
        let d = ClickPolicy::default();
        assert_eq!(policy.retry_ticks, 1);
        assert_eq!(policy.max_attempts, d.max_attempts);
        assert_eq!(policy.x_fraction, d.x_fraction);
        assert_eq!(policy.y_fraction, d.y_fraction);
    }
}
