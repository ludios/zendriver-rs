//! Playwright-style actionability predicates: visible / stable / enabled /
//! receives_pointer. The first three each run a small JS function on the
//! element's remote handle via `Element::call_on_main` and return a `bool`;
//! receives_pointer asks Chrome's own hit test (`DOM.getNodeForLocation`)
//! and runs JS only to tell whether the node hit is inside the element.
//!
//! `call_on_main` binds the element as the first positional argument, so
//! every body opens `function(el, ...)` and any caller-supplied arguments
//! follow it.
//!
//! The aggregate gate ([`wait_actionable`]) polls the four predicates in
//! order and raises `NotActionable` on deadline; `Element::click_with`,
//! `hover`, `focus`, `type_text`, and `screenshot` (in `element::actions`
//! / `element::screenshot`) each pick the [`ActionabilityCheck`] preset
//! matching what they need before dispatching.
//!
//! # A note on this module's unit tests
//!
//! The predicates are JavaScript, and `MockConnection` replays CDP frames
//! without a JS engine, so the `probe_source_*` tests below assert on the
//! *source text* each predicate puts on the wire. They pin that a clause is
//! present and spelled the way a past defect proved it has to be; they do
//! not execute it, and a mutation that rewrites a clause into something
//! equally well-formed but wrong passes them.
//!
//! The live-Chrome tests reach part of that. `tests/visibility.rs` runs per
//! PR and executes `check_visible` for both kinds of caller: the opacity
//! walk (through a slot and a shadow host), `content-visibility: auto`, and
//! the gate's viewport clause. `tests/element_world_regressions.rs` covers
//! the viewport clause in quirks mode and under a page-shrunk `innerHeight`.
//! `tests/find_visible_only.rs` covers `visible_only` with frames, but its
//! tests are `#[ignore]`d, so they run only in the scheduled
//! `nightly-ignored-tests` lane, which carries `continue-on-error: true`.
//!
//! `tests/actionability.rs` runs `check_enabled` on each case that its doc
//! lists, and checks that a click waits for its control to be enabled.
//!
//! `tests/actionability.rs` also runs `check_receives_pointer` on an element
//! in a closed shadow root, and on one that an overlay covers;
//! `tests/element_world_regressions.rs` clicks inside an iframe. Every
//! gated click in the live tests runs `check_stable`, but nothing checks
//! that it holds an animating element back; widening it means writing the
//! fixture that would catch the mistake.

use std::time::Duration;

use serde_json::json;
use tokio::time::Instant;

use crate::element::Element;
use crate::error::{Result, ZendriverError};

/// Set of actionability checks an action wants the element to satisfy
/// before its CDP dispatch. Per-field booleans gate the corresponding
/// `check_*` predicate in `wait_actionable`, and `hit_point` carries the
/// coordinate the hit-test should probe. Four named presets cover the
/// common combinations (`FULL`, `HOVER`, `VISIBLE_ONLY`, `TEXT_INPUT`);
/// callers may also construct ad-hoc sets directly, and a caller with an
/// explicit click position writes
/// `ActionabilityCheck { hit_point: opts.position, ..FULL }`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ActionabilityCheck {
    pub visible: bool,
    pub stable: bool,
    pub enabled: bool,
    pub receives_pointer: bool,
    /// Offset from the bbox top-left that [`check_receives_pointer`] should
    /// hit-test, matching [`crate::ClickOptions::position`]. `None` probes
    /// the bbox centre. Ignored unless `receives_pointer` is set.
    pub hit_point: Option<(f64, f64)>,
}

impl ActionabilityCheck {
    /// All four predicates — used by `click` and similar pointer-driven
    /// actions where layout stability + an unobstructed hit point matter.
    pub(crate) const FULL: Self = Self {
        visible: true,
        stable: true,
        enabled: true,
        receives_pointer: true,
        hit_point: None,
    };

    /// `FULL` minus the enabled check — used by `hover` / `hover_fast`.
    /// Hovering does not activate the element, so a disabled control still
    /// accepts `mouseover` and gating on `enabled` would reject a hover the
    /// browser performs happily.
    pub(crate) const HOVER: Self = Self {
        visible: true,
        stable: true,
        enabled: false,
        receives_pointer: true,
        hit_point: None,
    };

    /// Visibility only — used by `screenshot` (we just need pixels to
    /// capture; we don't care if a sibling overlay covers part of the
    /// element).
    pub(crate) const VISIBLE_ONLY: Self = Self {
        visible: true,
        stable: false,
        enabled: false,
        receives_pointer: false,
        hit_point: None,
    };

    /// Text-input combo — used by `type_text` / `focus` where the element
    /// must be visible + enabled but doesn't need a hit-tested pointer
    /// path (keystrokes route through the focused element, not the
    /// cursor's position).
    pub(crate) const TEXT_INPUT: Self = Self {
        visible: true,
        stable: false,
        enabled: true,
        receives_pointer: false,
        hit_point: None,
    };

    /// How many `Runtime.callFunctionOn` probes one pass of
    /// [`wait_actionable`] sends for this set when every probe passes: one
    /// per enabled JavaScript predicate (visible, enabled, stable). The hit
    /// test, when enabled, follows them, as CDP calls of its own.
    ///
    /// Exists so a test fixture can serve exactly the gate's calls without
    /// hard-coding a count per preset: a count that drifts from the set does
    /// not fail cleanly, it eats the *next* unrelated call and the failure
    /// surfaces at a later, unrelated `expect`.
    #[cfg(test)]
    pub(crate) const fn js_probe_count(self) -> usize {
        self.visible as usize + self.enabled as usize + self.stable as usize
    }
}

/// Where [`check_visible`] requires an element to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Within {
    /// No position test: an element below the fold, scrolled out of view
    /// inside a container, or placed off the page (`left: -9999px`) counts.
    /// For finding elements and `Element::is_visible`, which can't scroll.
    Anywhere,
    /// Inside the layout viewport now: for an action, after it has scrolled
    /// the element into view.
    Viewport,
}

/// `true` iff the element is rendered, and placed `within` the viewport
/// when asked. An element is visible when all of the following hold:
///
/// - it is attached to the document (`isConnected`);
/// - it passes the platform's own `Element.checkVisibility` test with
///   `opacityProperty` + `visibilityProperty`, which covers `display: none`,
///   `content-visibility: hidden`, `visibility: hidden` / `collapse`, and
///   `opacity: 0` on the element **or any ancestor**. Only for
///   [`Within::Viewport`] does it also pass `contentVisibilityAuto`, which
///   rejects content that a `content-visibility: auto` section skips
///   rendering while it is off screen;
/// - it has a positive bounding box;
/// - for [`Within::Viewport`], its bounding box **intersects** the viewport
///   — any overlap counts, so a partially-scrolled-in element still passes;
/// - its *effective* opacity — the product of its own computed opacity and
///   that of every ancestor in the flat tree (through slots and shadow
///   hosts) — is at least 1%.
///
/// The last two are the reason this isn't just a `checkVisibility` call.
/// `checkVisibility` only rejects an *exact* `opacity: 0`, so the standard
/// `opacity: 0.001` scraping honeypot passes it while being invisible to a
/// human; and `checkVisibility` says nothing about where the element sits
/// relative to the viewport.
///
/// Requires Chrome 121+, not 105+. `Element.checkVisibility` itself shipped
/// in 105, but the three option names passed below (`opacityProperty`,
/// `visibilityProperty`, `contentVisibilityAuto`) shipped in 121 — the first
/// two as aliases for 105's `checkOpacity` / `checkVisibilityCSS`, the third
/// as a new check (crbug feature 5070043440480256). Below 105 the call
/// throws, surfacing as a `JsException`: a loud failure rather than a
/// silently wrong answer. On 105–120 it does NOT throw — WebIDL drops
/// dictionary members it doesn't recognize — so the call silently degrades
/// to the default `display: none` test and stops covering
/// `visibility: hidden`/`collapse` and `content-visibility: auto`. The
/// effective-opacity loop below is independent of that and still holds.
///
/// Live callers: the `visible_only` filter in `FindBuilder`/`FindAllBuilder`
/// (`query::mod`) and `Element::is_visible`, with [`Within::Anywhere`]; the
/// actionability gate ([`wait_actionable`]), with [`Within::Viewport`].
pub(crate) async fn check_visible(el: &Element, within: Within) -> Result<bool> {
    let js = r#"
        function(el, inViewport) {
            if (!el || !el.isConnected) return false;

            // Platform primitive: handles `display: none`, `content-visibility`,
            // `visibility: hidden`/`collapse` and `opacity: 0` on the element OR any
            // ancestor — none of which the hand-rolled predecessor caught, since it
            // only ever read the element's own computed style, and its layout probe
            // reported a false negative for `position: fixed`.
            // `contentVisibilityAuto` only after scrolling: before it, a
            // `content-visibility: auto` section below the fold is skipped
            // and would read as hidden.
            if (!el.checkVisibility({
                opacityProperty: true,
                visibilityProperty: true,
                contentVisibilityAuto: inViewport
            })) return false;

            const rect = el.getBoundingClientRect();
            if (rect.width <= 0 || rect.height <= 0) return false;

            if (inViewport) {
                // On-screen test: the bbox must overlap the viewport by at least a
                // sliver. `getBoundingClientRect` is already viewport-relative, so
                // the box it must be compared against is the LAYOUT viewport. Two
                // things about reading that box are load-bearing:
                //
                // 1. It must never come from `window.inner*`, which a script
                //    can redefine, while `scroll_into_view` moves the element
                //    with Chrome's real viewport. Comparing a real-geometry
                //    scroll against a smaller, fabricated `innerHeight` (994
                //    against a real 1080, measured) leaves every element landing
                //    in the band between them permanently "not visible", which
                //    broke `focus` — and with it `type_text` / `press` /
                //    `type_keys` — on any below-the-fold field.
                //
                // 2. WHICH element reports the layout viewport depends on the
                //    rendering mode, and reading the wrong one fails silently.
                //    Standards mode: `documentElement`. Quirks (`BackCompat`):
                //    `documentElement.client*` reports the document's CONTENT box
                //    and `body.client*` reports the viewport. Measured on a 6000px
                //    doctype-less page: `documentElement.clientHeight` 6024,
                //    `body.clientHeight` 1080. Reading `documentElement`
                //    unconditionally therefore compares the bbox against the whole
                //    document on any page without a doctype — a no-op that fails
                //    open, on exactly the sloppy legacy pages most likely to be
                //    carrying a honeypot.
                const box = document.compatMode === 'BackCompat'
                    ? document.body
                    : document.documentElement;
                // The `BackCompat` arm can be null: a document whose `<body>` was
                // removed still renders elements appended to `documentElement`, and
                // still answers `isConnected` for them (reproduced against Chrome).
                // The fallback is `visualViewport` rather than `window.inner*`
                // because it is the other unspoofed source, and it reads the real
                // 1080 in both modes. It is the VISUAL viewport, which diverges
                // from the layout viewport under pinch-zoom or an on-screen
                // keyboard — neither reachable through this crate's emulation
                // surface, and neither triggerable by page script.
                const vw = box ? box.clientWidth : window.visualViewport.width;
                const vh = box ? box.clientHeight : window.visualViewport.height;
                if (rect.right <= 0 || rect.bottom <= 0 || rect.left >= vw || rect.top >= vh) {
                    return false;
                }
            }

            // Effective opacity: `checkVisibility` rejects only an exact `opacity: 0`,
            // so a near-zero honeypot (`opacity: 0.001`) sails through it. Multiply the
            // ancestor chain the way the compositor does and reject anything below 1%.
            // The threshold is deliberately not `> 0`: exact-zero is the case honeypots
            // avoid precisely because it is the one everyone already checks. 1% sits well
            // below anything a real UI renders at rest and well above the honeypot range.
            // Done last — `getComputedStyle` per ancestor is the priciest step here.
            // Opacity is in [0, 1], so the running product only ever decreases and
            // cannot climb back over the threshold: once it is under, stop walking.
            // The walk follows the flat tree, as rendering does: a slotted
            // element's parent is its slot, and a shadow root's is its host.
            // A `display: contents` node (every slot, by default) has no box,
            // so its opacity doesn't apply. Page script can't see a closed
            // shadow root's slots (`assignedSlot` is null), so the walk skips
            // that tree's wrappers and goes straight to the host.
            let effective = 1;
            for (let node = el; node;
                 node = node.assignedSlot || node.parentElement || node.getRootNode().host) {
                const computed = getComputedStyle(node);
                const own = parseFloat(computed.opacity);
                if (computed.display !== 'contents' && Number.isFinite(own)) effective *= own;
                if (effective < 0.01) return false;
            }

            return true;
        }
    "#;
    let in_viewport = within == Within::Viewport;
    let res = el
        .call_on_main(js, json!([{ "value": in_viewport }]))
        .await?;
    Ok(res.get("value").and_then(|v| v.as_bool()).unwrap_or(false))
}

/// `true` iff the element's bounding box is unchanged across two
/// `requestAnimationFrame` ticks (within 0.5px on each of x/y/w/h). This
/// catches mid-transition layout shifts that would race a synthesized
/// click.
pub(crate) async fn check_stable(el: &Element) -> Result<bool> {
    let js = r#"
        function(el) {
            return new Promise(resolve => {
                if (!el || !el.isConnected) { resolve(false); return; }
                const first = el.getBoundingClientRect();
                requestAnimationFrame(() => {
                    requestAnimationFrame(() => {
                        const second = el.getBoundingClientRect();
                        const stable =
                            Math.abs(first.x - second.x) < 0.5 &&
                            Math.abs(first.y - second.y) < 0.5 &&
                            Math.abs(first.width - second.width) < 0.5 &&
                            Math.abs(first.height - second.height) < 0.5;
                        resolve(stable);
                    });
                });
            });
        }
    "#;
    let res = el.call_on_main(js, json!([])).await?;
    Ok(res.get("value").and_then(|v| v.as_bool()).unwrap_or(false))
}

/// `true` iff the element is not disabled. Disabled means any of:
///
/// - The element matches CSS `:disabled`, which is Chrome's own answer.
///   Besides a control's own `disabled` attribute, it covers a control
///   inside a disabled `<fieldset>` (outside that fieldset's first
///   `<legend>`) and an `<option>` in a disabled `<optgroup>`.
/// - An ancestor that is not a `<fieldset>` matches `:disabled`, as a
///   disabled `<button>` does for a `<span>` inside it. (A disabled
///   fieldset counts only through the first rule, so that a control in its
///   first `<legend>` stays enabled.)
/// - Of the element and its ancestors, the nearest one whose
///   `aria-disabled` is `true` or `false` (in any mix of upper and lower
///   case) has `true`. So `aria-disabled="false"` on the element overrides
///   `aria-disabled="true"` on a container, as in Playwright.
/// - The element is an autonomous custom element (its name has a `-`) with
///   a `disabled` attribute. `:disabled` ignores such an element unless it
///   is form-associated, and its JavaScript `disabled` property, which the
///   page defines, is invisible from zendriver's isolated world, where this
///   check runs. Component libraries commonly reflect that property to the
///   attribute.
///
/// The ancestors are those in the element's own tree: the walk doesn't
/// leave the shadow root that contains the element.
pub(crate) async fn check_enabled(el: &Element) -> Result<bool> {
    // `Element.prototype.closest`, not `el.closest`: a form's named control
    // (`<input name="closest">`) hides the form's own method, and its
    // `localName`. The isolated world's `Element` is out of the page's
    // reach.
    let js = r#"
        function(el) {
            if (!el) return false;
            if (!(el instanceof Element)) return true;
            const proto = Element.prototype;
            if (proto.matches.call(el, ':disabled')) return false;
            const name = el.localName;
            const custom = typeof name === 'string' && name.includes('-');
            if (custom && proto.hasAttribute.call(el, 'disabled')) return false;
            if (proto.closest.call(el, ':disabled:not(fieldset)') !== null) return false;
            const aria = proto.closest.call(
                el, '[aria-disabled="true" i], [aria-disabled="false" i]');
            return aria === null || !proto.matches.call(aria, '[aria-disabled="true" i]');
        }
    "#;
    let res = el.call_on_main(js, json!([])).await?;
    Ok(res.get("value").and_then(|v| v.as_bool()).unwrap_or(false))
}

/// Whether the node with the backend id `node` is `el` or inside it.
///
/// Inside means reachable from `node` by walking up the flat tree as far as
/// the page's isolated world can see it: a pseudo-element counts as its
/// element, slotted content as inside its slot (only in an open shadow
/// root, since `assignedSlot` hides a closed one's), a shadow root's
/// content as inside its host, and an iframe's document as inside the
/// `<iframe>` (only for an iframe of the same origin, whose `frameElement`
/// the walk can read).
///
/// Resolves `node` into `el`'s world, whose context id `el`'s object id
/// holds, and walks up from it there. A node that Chrome resolves to no
/// object there (one in a document of another origin), or that is gone by
/// then, isn't inside `el`.
async fn is_inside(el: &Element, node: i64) -> Result<bool> {
    let context_id =
        crate::isolated_world::context_id_of_object(&el.remote_object_id_cloned().await?)?;
    let tab = el.tab();
    let resolved = tab
        .call(
            "DOM.resolveNode",
            json!({ "backendNodeId": node, "executionContextId": context_id }),
        )
        .await;
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(e) if crate::element::refresh::is_stale_node_error(&e) => return Ok(false),
        Err(e) => return Err(e),
    };
    let Some(object_id) = resolved["object"]["objectId"].as_str() else {
        return Ok(false);
    };
    // `Node.prototype`'s getter, not `n.parentNode`: a form's control named
    // `parentNode` hides the form's own.
    let js = r#"
        function(el, node) {
            const parentOf = Object.getOwnPropertyDescriptor(Node.prototype, 'parentNode').get;
            let n = node instanceof Node ? node : node.element;
            while (n) {
                if (n === el) return true;
                if (n instanceof Document) {
                    n = n.defaultView && n.defaultView.frameElement;
                } else {
                    n = (n instanceof Element && n.assignedSlot) || parentOf.call(n) || n.host;
                }
            }
            return false;
        }
    "#;
    let res = el
        .call_on_main(js, json!([{ "objectId": object_id }]))
        .await;
    // Best effort: a handle left behind only holds memory until its
    // document goes away.
    let _ = tab
        .call("Runtime.releaseObject", json!({ "objectId": object_id }))
        .await;
    Ok(res?.get("value").and_then(|v| v.as_bool()).unwrap_or(false))
}

/// `true` iff a click at the point that will actually be clicked would land
/// on the element or inside it (see [`is_inside`]). Returns `false` when
/// something else is at the point (an overlay, a sticky header, the element
/// itself with `pointer-events: none` letting the click through), or nothing
/// is (the point is outside the viewport).
///
/// Asks Chrome's own hit test, `DOM.getNodeForLocation`, at the point of the
/// page that `Input.dispatchMouseEvent` will use, to the nearest whole pixel
/// (the command takes integers): the element's
/// [`crate::Element::bounding_box`] plus `position` (or its centre), plus the
/// page's scroll offset, since `DOM.getNodeForLocation` takes a point of the
/// page rather than of the viewport. That hit test sees into shadow roots,
/// closed ones too, where `document.elementFromPoint` stops at the host; it
/// starts from the top frame of the tab's process, so an overlay in the top
/// page that covers an element in a same-process iframe counts; and, like a
/// real click, it passes through `pointer-events: none`.
///
/// `position` is the caller's offset from the bbox top-left, matching
/// [`crate::ClickOptions::position`]; `None` hit-tests the centre. Probing a
/// point the dispatch will not use lets the gate pass while the real click
/// lands on an overlay.
///
/// # Errors
///
/// The error of a CDP call, or [`ZendriverError::Navigation`] when
/// `Page.getLayoutMetrics` reports no scroll offset.
pub(crate) async fn check_receives_pointer(
    el: &Element,
    position: Option<(f64, f64)>,
) -> Result<bool> {
    let Some(bbox) = el.bounding_box().await? else {
        return Ok(false);
    };
    if bbox.width <= 0.0 || bbox.height <= 0.0 {
        return Ok(false);
    }
    let tab = el.tab();
    let metrics = tab.call("Page.getLayoutMetrics", json!({})).await?;
    let viewport = &metrics["cssLayoutViewport"];
    let (Some(scroll_x), Some(scroll_y)) = (viewport["pageX"].as_f64(), viewport["pageY"].as_f64())
    else {
        return Err(ZendriverError::Navigation(
            "Page.getLayoutMetrics returned no cssLayoutViewport scroll offset".into(),
        ));
    };
    let (dx, dy) = position.unwrap_or((bbox.width / 2.0, bbox.height / 2.0));
    let x = (bbox.x + dx + scroll_x).round();
    let y = (bbox.y + dy + scroll_y).round();
    // A point Chrome can't take (an offset that isn't finite, or beyond
    // `i32`) can't be clicked either.
    let in_range = |v: f64| v.is_finite() && v.abs() <= f64::from(i32::MAX);
    if !in_range(x) || !in_range(y) {
        return Ok(false);
    }
    let hit = tab
        .call(
            "DOM.getNodeForLocation",
            json!({ "x": x as i64, "y": y as i64 }),
        )
        .await;
    let hit = match hit {
        Ok(hit) => hit,
        // Nothing there: the point is outside the viewport.
        Err(ZendriverError::Cdp { ref message, .. }) if message.contains("No node found") => {
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    let Some(hit) = hit["backendNodeId"].as_i64() else {
        return Err(ZendriverError::Navigation(
            "DOM.getNodeForLocation returned no backendNodeId".into(),
        ));
    };
    if hit == el.backend_node_id_cloned().await? {
        return Ok(true);
    }
    is_inside(el, hit).await
}

/// Poll each predicate in `require` at 50 ms intervals until all enabled
/// checks pass, or `timeout` elapses. On deadline, returns
/// [`ZendriverError::NotActionable`] with the first-failing check's
/// human-readable reason ("not visible", "not enabled", "not stable (still
/// animating)", or "occluded by overlay").
///
/// Predicates are evaluated in fixed order: visible → enabled → stable →
/// receives_pointer — the two cheap reads first, so the expensive
/// two-frame stability wait and the hit-test never run while the element is
/// still hidden or disabled. The *set* of checks is Playwright's; this
/// ordering is not, and does not claim to be — Playwright's actionability
/// docs list visible → stable → receives events → enabled.
///
/// `require.hit_point` is the offset the caller will click at, forwarded to
/// the hit-test so the gate probes the point the dispatch actually uses.
/// `None` probes the bbox centre. It is re-read from the live bbox on every
/// poll, so an element that moves mid-wait is hit-tested where it currently
/// is.
pub(crate) async fn wait_actionable(
    el: &Element,
    require: ActionabilityCheck,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let poll_interval = Duration::from_millis(50);
    loop {
        let mut failed_reason: Option<&'static str> = None;
        if require.visible && !check_visible(el, Within::Viewport).await? {
            failed_reason = Some("not visible");
        } else if require.enabled && !check_enabled(el).await? {
            failed_reason = Some("not enabled");
        } else if require.stable && !check_stable(el).await? {
            failed_reason = Some("not stable (still animating)");
        } else if require.receives_pointer && !check_receives_pointer(el, require.hit_point).await?
        {
            failed_reason = Some("occluded by overlay");
        }
        match failed_reason {
            None => return Ok(()),
            Some(reason) => {
                if Instant::now() >= deadline {
                    return Err(ZendriverError::NotActionable(timeout, reason.to_owned()));
                }
                tokio::time::sleep(poll_interval).await;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::tab::Tab;
    use crate::test_support::{box_model_reply, expect, js_params, serve_call};
    use serde_json::Value;
    use zendriver_transport::SessionHandle;
    use zendriver_transport::testing::MockConnection;

    /// Collapse every run of whitespace to a single space, so a source
    /// assertion can span what the JS literal wraps across lines without
    /// pinning its indentation.
    fn one_line(js: &str) -> String {
        js.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Drive `check_visible` (for the viewport) against a mock connection,
    /// answer its probe with `value`, and hand back the JS source the probe
    /// actually sent plus the resolved verdict.
    async fn probe_check_visible(value: bool) -> (String, bool) {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess);
        let el = Element::from_jsret(tab.clone(), 99, "R1".to_string());

        let fut = tokio::spawn(async move { check_visible(&el, Within::Viewport).await });

        let call = serve_call(&mut mock, json!({ "value": value, "type": "boolean" })).await;
        assert_eq!(
            call["arguments"][1]["value"], true,
            "Within::Viewport must ask the probe for the viewport test"
        );

        let verdict = fut.await.unwrap().unwrap();
        conn.shutdown();
        let js = call["functionDeclaration"].as_str().unwrap().to_string();
        (js, verdict)
    }

    #[tokio::test]
    async fn probe_source_delegates_to_check_visibility() {
        let (js, verdict) = probe_check_visible(true).await;
        assert!(verdict, "a `true` probe result must resolve to visible");
        assert!(js.contains("checkVisibility"), "{js}");
        // The flags are what make `checkVisibility` cover ancestors —
        // without them it only reports `display: none`. A skipped
        // `content-visibility: auto` section counts as hidden only once the
        // element should be on screen.
        assert!(js.contains("opacityProperty: true"), "{js}");
        assert!(js.contains("visibilityProperty: true"), "{js}");
        assert!(js.contains("contentVisibilityAuto: inViewport"), "{js}");
    }

    #[tokio::test]
    async fn probe_source_tests_viewport_intersection() {
        let (js, _) = probe_check_visible(true).await;
        // An action requires the element on screen after scrolling it
        // there; that requires comparing the bbox against the viewport box.
        assert!(js.contains("inViewport"), "{js}");
        assert!(js.contains("rect.right <= 0"), "{js}");
        assert!(js.contains("rect.bottom <= 0"), "{js}");
        assert!(js.contains("rect.left >= vw"), "{js}");
        assert!(js.contains("rect.top >= vh"), "{js}");
    }

    /// The viewport box must be read from an unspoofed source, and from the
    /// element that actually reports the layout viewport in *both* rendering
    /// modes.
    ///
    /// Two separate defects are pinned here, both of which shipped:
    ///
    /// - Reading `window.inner*` at all. A script can redefine those, so
    ///   they must not appear — not even as a trailing fallback. The
    ///   predecessor of this test asserted only that `documentElement` came
    ///   *first*, which a
    ///   `documentElement.clientWidth || window.innerWidth` expression
    ///   satisfies while still reaching the spoofed value.
    /// - Reading `documentElement` unconditionally. In quirks mode that is
    ///   the document's content box (6024 on a 6000px page), which is
    ///   truthy, so the `||` fallback never fired and the whole on-screen
    ///   clause degraded to a no-op. Asserting merely that *some* viewport
    ///   is read would pass that.
    ///
    /// The whole-source substring search is safe because the probe's own
    /// comments spell the spoofable pair `window.inner*` rather than naming
    /// either property.
    ///
    /// The ternary is asserted whole rather than as three independent
    /// substrings. Naming `compatMode`, `body` and `documentElement`
    /// separately is satisfied just as well by the arms swapped the wrong way
    /// round — which is the exact defect the second bullet describes, so a
    /// pin that cannot tell the two apart pins nothing.
    #[tokio::test]
    async fn probe_source_reads_the_layout_viewport_in_both_rendering_modes() {
        let (js, _) = probe_check_visible(true).await;
        assert!(!js.contains("window.innerWidth"), "{js}");
        assert!(!js.contains("window.innerHeight"), "{js}");
        assert!(
            one_line(&js).contains(
                "document.compatMode === 'BackCompat' ? document.body : document.documentElement"
            ),
            "quirks mode must select `body` and standards mode `documentElement`, \
             in that order: {js}"
        );
    }

    #[tokio::test]
    async fn probe_source_multiplies_ancestor_opacity_against_a_threshold() {
        let (js, _) = probe_check_visible(true).await;
        // Honeypot guard: walk the ancestor chain and compare the product
        // against a non-zero floor, so `opacity: 0.001` (self OR ancestor)
        // reads as hidden.
        assert!(
            js.contains("node.assignedSlot || node.parentElement || node.getRootNode().host"),
            "the walk must follow the flat tree: {js}"
        );
        assert!(js.contains("parseFloat(computed.opacity)"), "{js}");
        assert!(js.contains("effective *= own"), "{js}");
        // The threshold test sits *inside* the walk: the product only ever
        // decreases, so a chain that has already gone under can stop early.
        assert!(
            one_line(&js).contains(
                "&& Number.isFinite(own)) effective *= own; if (effective < 0.01) return false; }"
            ),
            "the opacity floor must be checked inside the ancestor walk: {js}"
        );
    }

    #[tokio::test]
    async fn probe_source_drops_the_buggy_predecessors() {
        let (js, _) = probe_check_visible(true).await;
        // Regression pins for the three defects this probe replaced:
        // a string compare against '0' (missed `opacity: 0.001`), an
        // own-element-only style read (missed ancestor opacity), and the
        // `offsetParent` hack (false negative on `position: fixed`).
        assert!(!js.contains("offsetParent"), "{js}");
        assert!(!js.contains("style.opacity"), "{js}");
        assert!(!js.contains("=== '0'"), "{js}");
    }

    /// Not a source pin: this one covers the Rust side of `check_visible`,
    /// namely that it reads the probe's verdict out of `result.value` rather
    /// than defaulting to the `unwrap_or(false)` fallback.
    #[tokio::test]
    async fn check_visible_resolves_the_probe_result_it_was_handed() {
        let (_, hidden) = probe_check_visible(false).await;
        assert!(!hidden, "a `false` probe result must resolve to hidden");
        let (_, shown) = probe_check_visible(true).await;
        assert!(shown, "a `true` probe result must resolve to visible");
    }

    /// Drive one `check_receives_pointer` probe for an element with backend
    /// id 99 and the box of [`box_model_reply`], on a page scrolled down by
    /// 300 px, with the hit test answering `hit` (a backend node id, or the
    /// message of an error). The element's object id is `object_id`, and
    /// `at` is the click position (see [`check_receives_pointer`]). Hands
    /// back the point that the probe hit-tested, and the probe's task, for
    /// its verdict.
    async fn probe_pointer(
        mock: &mut MockConnection,
        conn: &zendriver_transport::Connection,
        object_id: &str,
        at: Option<(f64, f64)>,
        hit: Result<i64, &str>,
    ) -> (Value, tokio::task::JoinHandle<Result<bool>>) {
        let sess = SessionHandle::new(conn.clone(), "S1");
        let el = Element::from_jsret(Tab::new_for_test(sess), 99, object_id.to_string());
        let fut = tokio::spawn(async move { check_receives_pointer(&el, at).await });

        let id = expect(mock, "DOM.getBoxModel").await;
        mock.reply(id, box_model_reply()).await;
        let id = expect(mock, "Page.getLayoutMetrics").await;
        let viewport = json!({ "cssLayoutViewport": { "pageX": 0, "pageY": 300 } });
        mock.reply(id, viewport).await;
        let id = expect(mock, "DOM.getNodeForLocation").await;
        let point = mock.last_sent()["params"].clone();
        match hit {
            Ok(node) => {
                mock.reply(id, json!({ "backendNodeId": node, "frameId": "F" }))
                    .await
            }
            Err(message) => mock.reply_err(id, -32000, message).await,
        }
        (point, fut)
    }

    /// The hit test probes the point of the page that the click will use:
    /// the box's top-left corner plus the position (else the box's centre),
    /// plus the page's scroll offset. A hit on the element itself passes
    /// with no further call.
    #[tokio::test]
    async fn check_receives_pointer_hit_tests_the_clicks_point_of_the_page() {
        for (at, x, y) in [(None, 60, 345), (Some((3.0, 4.0)), 13, 324)] {
            let (mut mock, conn) = MockConnection::pair();
            let (point, fut) = probe_pointer(&mut mock, &conn, "1.5.1", at, Ok(99)).await;
            assert_eq!(point["x"], x, "{at:?}");
            assert_eq!(point["y"], y, "{at:?}");
            assert!(point.get("ignorePointerEventsNone").is_none(), "{point}");
            assert!(fut.await.unwrap().unwrap(), "{at:?}");
            conn.shutdown();
        }
    }

    /// Nothing at the point (outside the page) fails the hit test.
    #[tokio::test]
    async fn check_receives_pointer_fails_where_nothing_is() {
        let (mut mock, conn) = MockConnection::pair();
        let nothing = Err("No node found at given location");
        let (_, fut) = probe_pointer(&mut mock, &conn, "1.5.1", None, nothing).await;
        assert!(!fut.await.unwrap().unwrap());
        conn.shutdown();
    }

    /// A hit on another node passes iff that node is inside the element:
    /// the node is resolved into the element's world (whose context id the
    /// element's object id holds), the walk up from it runs there, and the
    /// node's handle is released.
    #[tokio::test]
    async fn check_receives_pointer_asks_whether_another_hit_is_inside() {
        for inside in [true, false] {
            let (mut mock, conn) = MockConnection::pair();
            let (_, fut) = probe_pointer(&mut mock, &conn, "1.5.1", None, Ok(7)).await;
            let id = expect(&mut mock, "DOM.resolveNode").await;
            assert_eq!(mock.last_sent()["params"]["backendNodeId"], 7);
            assert_eq!(mock.last_sent()["params"]["executionContextId"], 5);
            mock.reply(id, json!({ "object": { "objectId": "1.5.2" } }))
                .await;
            let call = serve_call(&mut mock, json!({ "value": inside, "type": "boolean" })).await;
            assert_eq!(
                call["arguments"],
                json!([{ "objectId": "1.5.1" }, { "objectId": "1.5.2" }])
            );
            assert_eq!(
                js_params(call["functionDeclaration"].as_str().unwrap()),
                ["el", "node"]
            );
            let id = expect(&mut mock, "Runtime.releaseObject").await;
            assert_eq!(mock.last_sent()["params"]["objectId"], "1.5.2");
            mock.reply(id, json!({})).await;
            assert_eq!(fut.await.unwrap().unwrap(), inside);
            conn.shutdown();
        }
    }

    /// Every JavaScript predicate binds the element as its first positional
    /// argument.
    ///
    /// `call_on_main` prepends the element handle to `arguments`, so a body
    /// that forgot its leading `el` parameter would silently read the handle
    /// as its first *caller* argument — and, for the two no-argument
    /// predicates, see `el` as `undefined` and answer `false` for everything.
    /// This drives all three past that in one go.
    ///
    /// The whole parameter list is asserted, not just the leading name: a
    /// `starts_with("function(el")` check also accepts `function(elephant)`.
    #[tokio::test]
    async fn every_predicate_binds_the_element_as_its_first_argument() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess);
        let el = Element::from_jsret(tab.clone(), 99, "R1".to_string());

        let fut = tokio::spawn({
            let e = el.clone();
            async move {
                (
                    check_visible(&e, Within::Anywhere).await.unwrap(),
                    check_enabled(&e).await.unwrap(),
                    check_stable(&e).await.unwrap(),
                )
            }
        });

        let mut calls = Vec::new();
        for _ in 0..3 {
            calls.push(serve_call(&mut mock, json!({ "value": true, "type": "boolean" })).await);
        }

        assert_eq!(fut.await.unwrap(), (true, true, true));

        let sources: Vec<&str> = calls
            .iter()
            .map(|c| c["functionDeclaration"].as_str().unwrap())
            .collect();

        // Pin which source belongs to which predicate, so a future reorder of
        // the gate can't quietly re-point one of these assertions.
        assert!(sources[0].contains("checkVisibility"), "{}", sources[0]);
        assert!(sources[1].contains("aria-disabled"), "{}", sources[1]);
        assert!(
            sources[2].contains("requestAnimationFrame"),
            "{}",
            sources[2]
        );

        // The visibility check binds `inViewport` after `el`; the other two
        // predicates bind `el` and nothing else.
        let expected = [vec!["el", "inViewport"], vec!["el"], vec!["el"]];
        for ((call, js), want) in calls.iter().zip(&sources).zip(expected) {
            assert_eq!(js_params(js), want, "{js}");
            assert_eq!(
                call["arguments"][0]["objectId"], "R1",
                "the element must be the first argument the body reads",
            );
        }
        assert_eq!(
            calls[0]["arguments"][1]["value"], false,
            "Within::Anywhere must not ask for the viewport test"
        );

        conn.shutdown();
    }
}
