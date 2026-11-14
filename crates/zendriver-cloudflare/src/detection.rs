//! Finding and measuring the Turnstile widget.
//!
//! Turnstile mounts its `<iframe>` inside a *closed* shadow root —
//! Cloudflare's own full-page challenge does — and no script in the page
//! can enter one. So the widget is found through CDP, which sees frames
//! rather than elements. The widget is a frame whose URL contains
//! [`iframe_src_contains`](TurnstileSelectors::iframe_src_contains), held
//! directly by the main frame, and it is listed in one of two places:
//!
//! - `Target.getTargets`, which lists a frame running in a process of its
//!   own as an `"iframe"` target, along with its parent frame's id. The
//!   cross-site widget is there under Chrome's default site isolation.
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
//! While the challenge is watched, the page can change under any one of
//! these calls: Cloudflare's full-page challenge navigates away once it is
//! passed, and a widget can be removed between two calls. Chrome then
//! answers a call with an error rather than failing it, and [`observe`]
//! reports that tick as `None`, to be polled again, instead of failing the
//! run.

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

/// The object group of the remote objects that measuring creates, so that
/// one `Runtime.releaseObjectGroup` frees them all.
const OBJECT_GROUP: &str = "zendriver-cloudflare";

/// What one poll tick saw.
#[derive(Debug)]
pub(crate) struct Observation {
    /// The non-empty value held by the first of the
    /// [`token_inputs`](TurnstileSelectors::token_inputs) to match.
    pub token: Option<String>,
    /// The light DOM holds the
    /// [`container`](TurnstileSelectors::container) or a token input.
    pub light_markers: bool,
    /// The widget's `<iframe>`, as a backend node id, when one is mounted.
    pub widget: Option<i64>,
    /// The widget's viewport rect, when the widget is a click target.
    pub clickable: Option<BoundingBox>,
    /// The `loaderId` of the main frame's document. It changes when a
    /// navigation replaces the document.
    pub document: String,
    /// The document is still being parsed.
    pub loading: bool,
}

impl Observation {
    /// Whether any marker at all is on the page: the container, a token
    /// input, or the widget.
    pub fn has_markers(&self) -> bool {
        self.light_markers || self.widget.is_some()
    }
}

/// `result`, with a CDP error answer turned into `Ok(None)`.
///
/// Chrome answering with an error means the page changed under the call
/// (see the module docs), which is a tick with nothing to report. Every
/// other failure — a closed connection, a detached session, a call that ran
/// out of time — still fails.
fn answered<T>(result: Result<T, CallError>) -> Result<Option<T>, CallError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(CallError::Rpc(..)) => Ok(None),
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

/// The `loaderId` of the main frame's document, and the widget's `<iframe>`
/// as a backend node id when one is mounted (see the module docs for where
/// it is looked for). `Ok(None)` when the page changed under the lookup.
async fn find_widget(
    session: &SessionHandle,
    marker: &str,
) -> Result<Option<(String, Option<i64>)>, CloudflareError> {
    let Some(tree) = answered(session.call("Page.getFrameTree", json!({})).await)? else {
        return Ok(None);
    };
    let Some(targets) = answered(session.call("Target.getTargets", json!({})).await)? else {
        return Ok(None);
    };
    let (main, document) = main_frame(&tree)?;
    for frame in widget_frames(&tree, &targets, main, marker) {
        let owner = session
            .call("DOM.getFrameOwner", json!({ "frameId": frame }))
            .await;
        // An error answer here is that frame having gone since the lists were
        // taken; another candidate may still be there.
        if let Some(node) = answered(owner)?.and_then(|o| o["backendNodeId"].as_i64()) {
            return Ok(Some((document.to_string(), Some(node))));
        }
    }
    Ok(Some((document.to_string(), None)))
}

/// `node`'s viewport rect when it is a click target (see
/// [`js::MEASURE_FN`]); `Ok(None)` when it is not one, or is gone.
pub(crate) async fn measure_widget(
    session: &SessionHandle,
    node: i64,
) -> Result<Option<BoundingBox>, CloudflareError> {
    let resolved = session
        .call(
            "DOM.resolveNode",
            json!({ "backendNodeId": node, "objectGroup": OBJECT_GROUP }),
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
    session
        .call(
            "Runtime.releaseObjectGroup",
            json!({ "objectGroup": OBJECT_GROUP }),
        )
        .await?;
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

/// Scroll `node` into view, centred and instantly, unless it is already
/// fully in view; then measure it as [`measure_widget`] does.
///
/// The scroll is CDP's (`DOM.scrollIntoViewIfNeeded`), so no script in the
/// page sees a `scrollIntoView` call, and no `scroll-behavior: smooth` on
/// the page turns it into an animation that the measurement would read
/// halfway through.
pub(crate) async fn scroll_widget_into_view(
    session: &SessionHandle,
    node: i64,
) -> Result<Option<BoundingBox>, CloudflareError> {
    let scrolled = session
        .call(
            "DOM.scrollIntoViewIfNeeded",
            json!({ "backendNodeId": node }),
        )
        .await;
    // An error answer is a widget gone or without a layout box, which the
    // measurement reports as `None` in its own right.
    answered(scrolled)?;
    measure_widget(session, node).await
}

/// Observe the page once: the token, the markers, the widget and the
/// document. `Ok(None)` when the page changed under the observation, which
/// says nothing about the challenge either way.
///
/// # Errors
/// [`CloudflareError::JsError`] when the poll evaluator threw, and
/// [`CloudflareError::Call`] when a CDP call failed rather than being
/// answered.
pub(crate) async fn observe(
    session: &SessionHandle,
    selectors: &TurnstileSelectors,
) -> Result<Option<Observation>, CloudflareError> {
    let evaluated = session
        .call(
            "Runtime.evaluate",
            json!({ "expression": js::poll_expr(selectors), "returnByValue": true }),
        )
        .await;
    let Some(reply) = answered(evaluated)? else {
        return Ok(None);
    };
    let poll: Poll = serde_json::from_value(completion_value(&reply)?)
        .map_err(|e| CloudflareError::JsError(format!("invalid poll payload: {e}")))?;

    let Some((document, widget)) = find_widget(session, &selectors.iframe_src_contains).await?
    else {
        return Ok(None);
    };
    let mut clickable = None;
    if let Some(node) = widget {
        clickable = measure_widget(session, node).await?;
    }
    Ok(Some(Observation {
        token: poll.token,
        light_markers: poll.markers,
        widget,
        clickable,
        document,
        loading: poll.loading,
    }))
}

impl CloudflareBypass<'_> {
    /// Returns `true` if a Turnstile challenge iframe is currently mounted —
    /// a frame whose URL contains this driver's
    /// [`iframe_src_contains`](TurnstileSelectors::iframe_src_contains),
    /// held directly by the main frame, in a shadow root (closed ones
    /// included) or not.
    ///
    /// # Errors
    /// [`CloudflareError::Call`] when a CDP call failed.
    pub async fn is_challenge_present(&self) -> Result<bool, CloudflareError> {
        let found = find_widget(self.session, &self.selectors.iframe_src_contains).await?;
        Ok(found.is_some_and(|(_, widget)| widget.is_some()))
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    const MARKER: &str = "challenges.cloudflare.com";

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
