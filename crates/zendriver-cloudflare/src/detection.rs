//! Finding and measuring the Turnstile widget.
//!
//! Turnstile mounts its `<iframe>` inside a *closed* shadow root —
//! Cloudflare's own full-page challenge does — which a script can reach
//! only through the reference `attachShadow` handed to the script that made
//! it, never by walking the DOM. So the widget is found through CDP, which
//! sees frames rather than elements. The widget is a frame whose URL
//! contains [`iframe_src_contains`](TurnstileSelectors::iframe_src_contains),
//! held directly by the main frame, and it is listed in one of two places:
//!
//! - `Target.getTargets`, which lists a frame running in a process of its
//!   own as an `"iframe"` target, along with its parent frame's id (from
//!   Chrome 141 on). The cross-site widget is there under Chrome's default
//!   site isolation.
//! - `Page.getFrameTree`, which lists the frames running in the page's own
//!   process. The widget is there when site isolation is off (as with
//!   `--disable-site-isolation-trials`).
//!
//! `DOM.getFrameOwner` then names the `<iframe>` element holding that
//! frame, whether or not it is in a shadow root. Only *direct* children of
//! the main frame count, because the click is aimed in the main frame's
//! viewport, and that is the viewport a direct child's `<iframe>` is
//! measured in.
//!
//! Everything this crate runs in the page runs in an isolated world (see
//! `WORLD_NAME`) rather than in the page's main world, where Cloudflare's
//! challenge script runs and can watch what is called.
//!
//! While the challenge is watched, the page can change under any one of
//! these calls: Cloudflare's full-page challenge navigates away when it is
//! passed, and a widget can be removed between two calls. Chrome then
//! answers the call with a server error rather than a result, and an
//! observation that gets one reports nothing, to be made again on the next
//! tick, instead of failing the run.

use serde::Deserialize;
use serde_json::{Value, json};
use zendriver_transport::{CallError, SessionHandle};

use crate::CloudflareBypass;
use crate::error::CloudflareError;
use crate::js;
use crate::options::TurnstileSelectors;

/// Bounding box of the Turnstile iframe, in viewport CSS pixels.
///
/// Public because it is what a caller-supplied
/// [`on_click`](CloudflareBypass::on_click) handler is measured against, as
/// [`ClickTarget::bbox`](crate::ClickTarget::bbox). The `zendriver` facade
/// re-exports it as `TurnstileBoundingBox`, since that crate already has a
/// `BoundingBox` of its own for element geometry.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct BoundingBox {
    /// Viewport-relative left edge, in CSS pixels.
    pub x: f64,
    /// Viewport-relative top edge, in CSS pixels.
    pub y: f64,
    /// Width in CSS pixels.
    pub width: f64,
    /// Height in CSS pixels.
    pub height: f64,
}

/// The JSON-RPC code of Chrome's server errors, which is what it answers
/// with when the context, node, object or frame a call names went away
/// under it.
const SERVER_ERROR: i32 = -32000;

/// The JSON-RPC code for invalid parameters.
const INVALID_PARAMS: i32 = -32602;

/// `result`, with a server-error answer turned into `Ok(None)`.
///
/// A server error here means the page changed under the call (see the
/// module docs), which is a tick with nothing to report. Every other
/// failure still fails: an unknown method or invalid parameters, a closed
/// connection, a detached session, a call that ran out of time.
fn answered<T>(result: Result<T, CallError>) -> Result<Option<T>, CallError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(CallError::Rpc(SERVER_ERROR, ..)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The completion value of a `Runtime.evaluate` or `Runtime.callFunctionOn`
/// reply, or [`CloudflareError::JsError`] when the script threw.
fn completion_value(reply: &Value) -> Result<Value, CloudflareError> {
    if let Some(details) = reply.get("exceptionDetails") {
        let message = details["exception"]["description"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();
        return Err(CloudflareError::JsError(message));
    }
    Ok(reply["result"]["value"].clone())
}

/// The name of the isolated world this crate's scripts run in.
///
/// Cloudflare's challenge script runs in the page's main world, where it
/// can wrap any built-in — `querySelector`, `getBoundingClientRect` — and
/// see who calls it. Observed live on Chrome 157 (November 2026): while
/// the poll queried the token input from the main world, Cloudflare took
/// every click and then served its challenge again; with the poll in an
/// isolated world, the first click passed. A script in an isolated world
/// has built-ins of its own, which no page script can reach.
const WORLD_NAME: &str = "zendriver-cloudflare";

/// The execution context id of this crate's isolated world in the main
/// frame `main`'s current document; `Ok(None)` when the page changed under
/// the call.
///
/// Asked for on every tick. Chrome keeps one world per name, with one
/// context in each document, so a repeated call on the same document
/// returns the same context and creates nothing — and a call after a
/// navigation returns the new document's, which a cached id would not.
async fn isolated_world(
    session: &SessionHandle,
    main: &str,
) -> Result<Option<i64>, CloudflareError> {
    let made = session
        .call(
            "Page.createIsolatedWorld",
            json!({ "frameId": main, "worldName": WORLD_NAME }),
        )
        .await;
    // Chrome answers a frame id that went away meanwhile with invalid
    // parameters rather than a server error.
    let made = match made {
        Err(CallError::Rpc(INVALID_PARAMS, message, _)) if message.contains("No frame") => {
            return Ok(None);
        }
        made => answered(made)?,
    };
    let Some(made) = made else {
        return Ok(None);
    };
    match made["executionContextId"].as_i64() {
        Some(context) => Ok(Some(context)),
        None => Err(CloudflareError::JsError(format!(
            "Page.createIsolatedWorld named no context: {made}"
        ))),
    }
}

/// What one poll tick saw.
#[derive(Debug)]
pub(crate) struct Observation {
    /// The non-empty value held by the first of the
    /// [`token_inputs`](TurnstileSelectors::token_inputs) to have one.
    pub token: Option<String>,
    /// The light DOM holds the
    /// [`container`](TurnstileSelectors::container) or a token input.
    pub light_markers: bool,
    /// The widget's `<iframe>`, as a backend node id, when one is mounted.
    /// Not looked for once a token has shown.
    pub widget: Option<i64>,
    /// The `loaderId` of the main frame's document. It changes when a
    /// navigation replaces the document.
    pub document: String,
    /// The document is still being parsed.
    pub loading: bool,
    /// The isolated world's context the observation ran its script in, for
    /// the measurements before a click to run in too.
    pub context: i64,
}

impl Observation {
    /// Whether any marker at all is on the page: the container, a token
    /// input, or the widget.
    pub fn has_markers(&self) -> bool {
        self.light_markers || self.widget.is_some()
    }
}

/// The poll evaluator's reply.
#[derive(Debug, Deserialize)]
struct Poll {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    markers: bool,
    #[serde(default)]
    loading: bool,
}

/// The main frame's id and the `loaderId` of its document, from a
/// `Page.getFrameTree` reply.
fn main_frame(tree: &Value) -> Result<(&str, &str), CloudflareError> {
    let frame = &tree["frameTree"]["frame"];
    match (frame["id"].as_str(), frame["loaderId"].as_str()) {
        (Some(id), Some(loader)) => Ok((id, loader)),
        _ => Err(CloudflareError::JsError(format!(
            "Page.getFrameTree named no main frame: {tree}"
        ))),
    }
}

/// The ids of the frames that may be the widget: the main frame's direct
/// children whose URL contains `marker`, from a `Page.getFrameTree` reply
/// (in-process frames) and a `Target.getTargets` reply (frames in a process
/// of their own). An empty `marker` matches no frame, since every URL
/// contains the empty string.
fn widget_frames(tree: &Value, targets: &Value, main: &str, marker: &str) -> Vec<String> {
    if marker.is_empty() {
        return Vec::new();
    }
    let mut frames = Vec::new();
    for child in tree["frameTree"]["childFrames"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let frame = &child["frame"];
        if frame["url"]
            .as_str()
            .is_some_and(|url| url.contains(marker))
        {
            if let Some(id) = frame["id"].as_str() {
                frames.push(id.to_string());
            }
        }
    }
    for target in targets["targetInfos"].as_array().into_iter().flatten() {
        if target["type"] == "iframe"
            && target["parentFrameId"] == main
            && target["url"]
                .as_str()
                .is_some_and(|url| url.contains(marker))
        {
            if let Some(id) = target["targetId"].as_str() {
                frames.push(id.to_string());
            }
        }
    }
    frames
}

/// The widget's `<iframe>`, as a backend node id, when one is mounted in
/// the page whose `Page.getFrameTree` reply is `tree` and whose main frame
/// is `main` (see the module docs for where it is looked for).
async fn find_widget(
    session: &SessionHandle,
    tree: &Value,
    main: &str,
    marker: &str,
) -> Result<Option<i64>, CloudflareError> {
    // An explicit filter, or Chrome applies whatever discovery filter the
    // session was given, which can leave the iframes out. A server error
    // here is no navigation (the list is the browser's) but a refusal, such
    // as a client that may not list targets, so it fails the run.
    let filter = json!({ "filter": [{ "type": "iframe" }] });
    let targets = session.call("Target.getTargets", filter).await?;
    for frame in widget_frames(tree, &targets, main, marker) {
        let owner = session
            .call("DOM.getFrameOwner", json!({ "frameId": frame }))
            .await;
        // A server error here is that frame having gone since the lists were
        // taken, or belonging to another page; another candidate may still
        // be there.
        if let Some(node) = answered(owner)?.and_then(|o| o["backendNodeId"].as_i64()) {
            return Ok(Some(node));
        }
    }
    Ok(None)
}

/// Scroll the widget `node` into view, unless it is already fully in view,
/// then measure it as [`measure_widget`] does.
///
/// The scroll is CDP's `DOM.scrollIntoViewIfNeeded`, which is instant
/// whatever `scroll-behavior` the page sets, so the measurement reads where
/// the widget ended up. It calls no page-overridable method, although the
/// page still sees a `scroll` event and its new scroll position.
pub(crate) async fn scroll_widget_into_view(
    session: &SessionHandle,
    node: i64,
    context: i64,
) -> Result<Option<BoundingBox>, CloudflareError> {
    let scrolled = session
        .call(
            "DOM.scrollIntoViewIfNeeded",
            json!({ "backendNodeId": node }),
        )
        .await;
    // A server error is a widget gone or without a layout box, which the
    // measurement reports as `None` in its own right.
    answered(scrolled)?;
    measure_widget(session, node, context).await
}

/// The viewport rect of the widget `node` when it is a click target (see
/// [`js::MEASURE_FN`], run in the isolated world `context`); `Ok(None)`
/// when it is not one, or is gone.
pub(crate) async fn measure_widget(
    session: &SessionHandle,
    node: i64,
    context: i64,
) -> Result<Option<BoundingBox>, CloudflareError> {
    let resolved = session
        .call(
            "DOM.resolveNode",
            json!({ "backendNodeId": node, "executionContextId": context }),
        )
        .await;
    let Some(object) = answered(resolved)? else {
        return Ok(None);
    };
    let Some(object_id) = object["object"]["objectId"].as_str() else {
        return Ok(None);
    };
    let measured = session
        .call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": js::MEASURE_FN,
                "returnByValue": true,
            }),
        )
        .await;
    let released = session
        .call("Runtime.releaseObject", json!({ "objectId": object_id }))
        .await;
    answered(released)?;
    let Some(reply) = answered(measured)? else {
        return Ok(None);
    };
    let value = completion_value(&reply)?;
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| CloudflareError::JsError(format!("invalid widget rect: {e}")))
}

/// Observe the page once: the token, the markers, the widget and the
/// document, running the poll evaluator in this crate's isolated world.
/// `Ok(None)` when the page changed under the observation, which says
/// nothing about the challenge either way.
///
/// # Errors
/// [`CloudflareError::JsError`] when the poll evaluator threw or a reply
/// could not be decoded, and [`CloudflareError::Call`] when a CDP call
/// failed rather than being answered.
pub(crate) async fn observe(
    session: &SessionHandle,
    selectors: &TurnstileSelectors,
) -> Result<Option<Observation>, CloudflareError> {
    let Some(tree) = answered(session.call("Page.getFrameTree", json!({})).await)? else {
        return Ok(None);
    };
    let (main, document) = main_frame(&tree)?;
    // The world's context is `document`'s, so the evaluator reads `document`,
    // or answers with a server error if a navigation replaced it meanwhile.
    let Some(context) = isolated_world(session, main).await? else {
        return Ok(None);
    };
    let evaluated = session
        .call(
            "Runtime.evaluate",
            json!({
                "expression": js::poll_expr(selectors),
                "contextId": context,
                "returnByValue": true,
            }),
        )
        .await;
    let Some(reply) = answered(evaluated)? else {
        return Ok(None);
    };
    let poll: Poll = serde_json::from_value(completion_value(&reply)?)
        .map_err(|e| CloudflareError::JsError(format!("invalid poll payload: {e}")))?;

    // A token ends the run, so a token in hand skips the lookup, which could
    // otherwise lose it to a server error.
    let mut widget = None;
    if poll.token.is_none() {
        widget = find_widget(session, &tree, main, &selectors.iframe_src_contains).await?;
    }
    Ok(Some(Observation {
        token: poll.token,
        light_markers: poll.markers,
        widget,
        document: document.to_string(),
        loading: poll.loading,
        context,
    }))
}

impl CloudflareBypass<'_> {
    /// Returns `true` if a Turnstile challenge iframe is currently mounted —
    /// a frame whose URL contains this driver's
    /// [`iframe_src_contains`](TurnstileSelectors::iframe_src_contains),
    /// held directly by the main frame, in a shadow root (closed ones
    /// included) or not. `false` too when the page was between documents.
    ///
    /// # Errors
    /// [`CloudflareError::Call`] when a CDP call failed, and
    /// [`CloudflareError::JsError`] when Chrome's frame tree named no main
    /// frame.
    pub async fn is_challenge_present(&self) -> Result<bool, CloudflareError> {
        let tree = self.session.call("Page.getFrameTree", json!({})).await;
        let Some(tree) = answered(tree)? else {
            return Ok(false);
        };
        let (main, _) = main_frame(&tree)?;
        let marker = &self.selectors.iframe_src_contains;
        Ok(find_widget(self.session, &tree, main, marker)
            .await?
            .is_some())
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    const MARKER: &str = "challenges.cloudflare.com";

    /// A `Page.getFrameTree` reply: main frame `MAIN` on document `DOC1`,
    /// with in-process child frames given as `(frame id, URL)` pairs.
    fn tree(children: &[(&str, &str)]) -> Value {
        let children: Vec<Value> = children
            .iter()
            .map(|(id, url)| json!({ "frame": { "id": id, "url": url } }))
            .collect();
        json!({
            "frameTree": {
                "frame": { "id": "MAIN", "loaderId": "DOC1" },
                "childFrames": children,
            }
        })
    }

    /// A `Target.getTargets` reply listing `infos`.
    fn targets(infos: &[Value]) -> Value {
        json!({ "targetInfos": infos })
    }

    /// With site isolation off, the widget is an ordinary child frame.
    #[test]
    fn an_in_process_child_frame_with_the_marker_is_a_candidate() {
        let tree = tree(&[
            ("ADS", "https://ads.example/slot"),
            (
                "CF",
                "https://challenges.cloudflare.com/cdn-cgi/challenge-platform/x",
            ),
        ]);
        assert_eq!(
            widget_frames(&tree, &targets(&[]), "MAIN", MARKER),
            vec!["CF".to_string()]
        );
    }

    /// With site isolation on, the widget is an `"iframe"` target of its own,
    /// absent from the frame tree. Its parent must be the main frame: a
    /// widget in another tab, or nested in a frame of this page, is
    /// measured in some other viewport.
    #[test]
    fn an_out_of_process_frame_counts_only_as_a_direct_child_of_the_main_frame() {
        let infos = [
            json!({ "type": "iframe", "targetId": "MINE", "parentFrameId": "MAIN", "url": format!("https://{MARKER}/a") }),
            json!({ "type": "iframe", "targetId": "OTHER_TAB", "parentFrameId": "ELSEWHERE", "url": format!("https://{MARKER}/b") }),
            json!({ "type": "page", "targetId": "PAGE", "url": format!("https://{MARKER}/c") }),
            json!({ "type": "iframe", "targetId": "ADS", "parentFrameId": "MAIN", "url": "https://ads.example/" }),
        ];
        assert_eq!(
            widget_frames(&tree(&[]), &targets(&infos), "MAIN", MARKER),
            vec!["MINE".to_string()]
        );
    }

    /// `"".contains` is true of every URL, so an empty marker taken
    /// literally would hand the click to the page's first frame, whatever it
    /// is. It switches the iframe signal off instead, as an empty
    /// `container` does for the container signal.
    #[test]
    fn an_empty_marker_matches_no_frame() {
        let tree = tree(&[("ADS", "https://ads.example/slot")]);
        let infos = [
            json!({ "type": "iframe", "targetId": "T", "parentFrameId": "MAIN", "url": "https://x.example/" }),
        ];
        assert!(widget_frames(&tree, &targets(&infos), "MAIN", "").is_empty());
    }

    /// The presence probe asks Chrome, so it finds the widget in either
    /// process layout, and reports a page without one as such.
    #[tokio::test]
    async fn is_challenge_present_finds_the_widget_frame_wherever_it_runs() {
        use crate::testutil::{Page, rect, serve};
        use zendriver_transport::testing::MockConnection;

        let widget = rect(10.0, 20.0, 300.0, 65.0);
        for (page, expected) in [
            (Page::challenge().widget_at(widget.clone()), true),
            (Page::challenge().widget_at(widget).out_of_process(), true),
            (Page::challenge(), false),
        ] {
            let (mut mock, conn) = MockConnection::pair();
            let sess = SessionHandle::new(conn.clone(), "S1");
            let fut =
                tokio::spawn(
                    async move { CloudflareBypass::new(&sess).is_challenge_present().await },
                );
            serve(&mut mock, |_| page.clone()).await;
            assert_eq!(fut.await.unwrap().unwrap(), expected, "{page:?}");
            conn.shutdown();
        }
    }
}
