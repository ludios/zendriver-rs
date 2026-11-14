//! Test-only helpers shared by this crate's unit tests.
//!
//! Two pieces: a bounded way to await one CDP command, and a fake page that
//! answers every command the driver sends.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use serde_json::{Value, json};
use zendriver_transport::testing::MockConnection;

/// How long a single awaited command may take to arrive before the test
/// fails.
///
/// [`MockConnection::expect_cmd`] has no timeout of its own, so a bare use of
/// it turns a refactor that drops a CDP call into a hang rather than a
/// failure — the test never fails, it just never finishes, and CI reports a
/// job timeout with no failing test name in it.
pub(crate) const CMD_BUDGET: Duration = Duration::from_secs(5);

/// How long [`serve`] waits for the driver's next command before deciding
/// the run is over. Long enough to cover a poll interval plus scheduling
/// jitter, short enough that a finished driver ends the loop promptly.
pub(crate) const DRAIN_BUDGET: Duration = Duration::from_millis(300);

/// Bounded [`MockConnection::expect_cmd`] — see [`CMD_BUDGET`].
///
/// Bounding it fixes the hang, not the weakness: `expect_cmd` silently
/// discards every frame that does not match, so it still only proves the
/// command *eventually* arrived. It can never establish that a command came
/// next, nor that some other command was absent. Assertions about ordering
/// or absence need [`serve`], which sees every frame.
pub(crate) async fn expect_cmd(mock: &mut MockConnection, method: &str) -> u64 {
    match tokio::time::timeout(CMD_BUDGET, mock.expect_cmd(method)).await {
        Ok(id) => id,
        Err(_) => panic!("timed out waiting for {method}"),
    }
}

/// A viewport rect, as the measuring function returns it.
pub(crate) fn rect(x: f64, y: f64, w: f64, h: f64) -> Value {
    json!({ "x": x, "y": y, "width": w, "height": h })
}

/// URL of the widget's frame on the fake page.
pub(crate) const WIDGET_URL: &str =
    "https://challenges.cloudflare.com/cdn-cgi/challenge-platform/turnstile";

/// Backend node id of the widget's `<iframe>` on the fake page.
pub(crate) const WIDGET_NODE: i64 = 42;

/// The widget on a [`Page`].
#[derive(Debug, Clone)]
pub(crate) enum Widget {
    /// No widget is mounted.
    Absent,
    /// A widget is mounted, and measures as this rect: `Value::Null` for a
    /// widget that is not a click target.
    Mounted(Value),
}

/// What the fake page shows during one poll tick.
#[derive(Debug, Clone)]
pub(crate) struct Page {
    /// The value in the token input, if any.
    pub token: Option<&'static str>,
    /// The light DOM holds a container or a token input.
    pub light_markers: bool,
    pub widget: Widget,
    /// The widget's frame runs in a process of its own (site isolation on),
    /// so `Target.getTargets` lists it and the frame tree does not.
    pub out_of_process: bool,
    /// The `loaderId` of the main frame's document.
    pub document: &'static str,
    /// The document is still being parsed.
    pub loading: bool,
    /// The error Chrome answers the poll evaluator with, as a JSON-RPC code
    /// and message, instead of a result.
    pub poll_error: Option<(i32, &'static str)>,
}

impl Page {
    /// A challenge page on document `D1`: a token input in the light DOM,
    /// no widget yet, no token.
    pub fn challenge() -> Self {
        Self {
            token: None,
            light_markers: true,
            widget: Widget::Absent,
            out_of_process: false,
            document: "D1",
            loading: false,
            poll_error: None,
        }
    }

    /// The same page with a widget that measures `at`.
    pub fn widget_at(mut self, at: Value) -> Self {
        self.widget = Widget::Mounted(at);
        self
    }

    pub fn token(mut self, token: &'static str) -> Self {
        self.token = Some(token);
        self
    }

    /// The same page without a container or a token input.
    pub fn no_light_markers(mut self) -> Self {
        self.light_markers = false;
        self
    }

    pub fn document(mut self, document: &'static str) -> Self {
        self.document = document;
        self
    }

    pub fn loading(mut self) -> Self {
        self.loading = true;
        self
    }

    pub fn out_of_process(mut self) -> Self {
        self.out_of_process = true;
        self
    }

    /// The same page while a navigation replaces it: Chrome answers the poll
    /// evaluator with a server error.
    pub fn between_documents(mut self) -> Self {
        self.poll_error = Some((-32000, "Cannot find context with specified id"));
        self
    }

    /// The same page in a Chrome that answers the poll evaluator with a
    /// non-server error, which no navigation explains.
    pub fn refusing_the_poll(mut self) -> Self {
        self.poll_error = Some((-32601, "'Runtime.evaluate' wasn't found"));
        self
    }

    /// The context id of the isolated world in this page's document: one
    /// per document, as Chrome has it (`D1` → 1, `D2` → 2).
    fn context(&self) -> i64 {
        self.document.trim_start_matches('D').parse().unwrap()
    }

    /// Chrome's reply to `method` with `params`, on this page. Asserts that
    /// the poll and the measurements run in the document's isolated world.
    fn reply(&self, method: &str, params: &Value) -> Value {
        let world = self.context();
        let mounted = matches!(self.widget, Widget::Mounted(_));
        match method {
            "Page.createIsolatedWorld" => {
                assert_eq!(params["frameId"], "MAIN");
                json!({ "executionContextId": world })
            }
            "Runtime.evaluate" => {
                assert_eq!(params["contextId"], world, "the poll ran outside the world");
                json!({ "result": { "type": "object", "value": {
                "token": self.token,
                "markers": self.light_markers,
                "loading": self.loading,
            } } })
            }
            "Page.getFrameTree" => {
                let mut children = Vec::new();
                if mounted && !self.out_of_process {
                    children.push(json!({ "frame": { "id": "WIDGET", "url": WIDGET_URL } }));
                }
                json!({ "frameTree": {
                    "frame": { "id": "MAIN", "loaderId": self.document },
                    "childFrames": children,
                } })
            }
            "Target.getTargets" => {
                assert_eq!(params["filter"][0]["type"], "iframe");
                let mut infos = vec![
                    json!({ "type": "page", "targetId": "MAIN", "url": "https://site.example/" }),
                ];
                if mounted && self.out_of_process {
                    infos.push(json!({
                        "type": "iframe", "targetId": "WIDGET", "parentFrameId": "MAIN", "url": WIDGET_URL,
                    }));
                }
                json!({ "targetInfos": infos })
            }
            "DOM.getFrameOwner" => {
                assert_eq!(params["frameId"], "WIDGET");
                json!({ "backendNodeId": WIDGET_NODE })
            }
            "DOM.resolveNode" => {
                assert_eq!(params["backendNodeId"], WIDGET_NODE);
                assert_eq!(
                    params["executionContextId"], world,
                    "measured outside the world"
                );
                json!({ "object": { "type": "object", "objectId": "WIDGET_OBJECT" } })
            }
            "Runtime.callFunctionOn" => {
                assert_eq!(params["objectId"], "WIDGET_OBJECT");
                let Widget::Mounted(at) = &self.widget else {
                    panic!("measured a widget the page does not have");
                };
                json!({ "result": { "type": "object", "value": at } })
            }
            _ => json!({}),
        }
    }
}

/// One CDP command the driver sent, with the poll tick it belongs to.
#[derive(Debug, Clone)]
pub(crate) struct Call {
    /// The poll tick: each `Page.getFrameTree` (a tick's first command)
    /// opens one, and the commands after it belong to it.
    pub tick: usize,
    /// The method, with the event `type` appended for mouse events —
    /// `Input.dispatchMouseEvent(mousePressed)` — so that a test can count
    /// clicks rather than the three events each one is made of.
    pub label: String,
    pub params: Value,
}

/// Labels of the calls a test asks about. [`saw`] matches on a prefix, so
/// [`MOUSE`] matches every mouse event.
pub(crate) const SCROLL: &str = "DOM.scrollIntoViewIfNeeded";
pub(crate) const MOUSE: &str = "Input.dispatchMouseEvent";
pub(crate) const PRESS: &str = "Input.dispatchMouseEvent(mousePressed)";

/// Answer every command the driver sends from the fake page `page(tick)`,
/// until the driver goes quiet for [`DRAIN_BUDGET`].
///
/// Returns every command observed, in order, so a test can assert not only
/// what happened but when, and what was **absent** — which
/// [`expect_cmd`] structurally cannot do, since it discards non-matching
/// frames.
pub(crate) async fn serve(mock: &mut MockConnection, page: impl Fn(usize) -> Page) -> Vec<Call> {
    let mut calls: Vec<Call> = Vec::new();
    let mut polls = 0usize;
    let mut tick = 0usize;
    let mut current = page(0);

    while let Some((method, id)) = mock.recv_cmd_timeout(DRAIN_BUDGET).await {
        let params = mock.last_sent()["params"].clone();
        if method == "Page.getFrameTree" {
            tick = polls;
            polls += 1;
            current = page(tick);
        }
        let mut label = method.clone();
        if method == MOUSE {
            label = format!("{method}({})", params["type"].as_str().unwrap());
        }
        calls.push(Call {
            tick,
            label,
            params: params.clone(),
        });
        let poll_error = current.poll_error.filter(|_| method == "Runtime.evaluate");
        if let Some((code, message)) = poll_error {
            mock.reply_err(id, code, message).await;
            continue;
        }
        mock.reply(id, current.reply(&method, &params)).await;
    }
    calls
}

/// The polls `calls` holds: one per tick the driver ran.
pub(crate) fn polls(calls: &[Call]) -> usize {
    calls
        .iter()
        .filter(|call| call.label == "Page.getFrameTree")
        .count()
}

/// The tick and the `(x, y)` of each click, in order.
pub(crate) fn presses(calls: &[Call]) -> Vec<(usize, (f64, f64))> {
    calls
        .iter()
        .filter(|call| call.label == PRESS)
        .map(|call| {
            let at = (
                call.params["x"].as_f64().unwrap(),
                call.params["y"].as_f64().unwrap(),
            );
            (call.tick, at)
        })
        .collect()
}

/// True if any command whose label starts with `prefix` was observed.
pub(crate) fn saw(calls: &[Call], prefix: &str) -> bool {
    calls.iter().any(|call| call.label.starts_with(prefix))
}
