//! [`Element`] read methods: attribute access, inner/outer markup, layout
//! geometry, and visibility/enabled state.
//!
//! Each method routes through an internal "refresh on stale" wrapper so a
//! stale CDP handle (post-navigation, post-React-rerender) triggers exactly
//! one transparent re-resolve and retry. `is_visible` and `is_enabled` are
//! thin wrappers around the actionability predicates that the actionability
//! gate also consumes — keeping a single source of truth for "what counts
//! as visible/enabled."
//!
//! `bounding_box` returns a viewport-relative [`BoundingBox`] derived from
//! the `border` quad of `DOM.getBoxModel`. Returns `Ok(None)` when the
//! node has no box (e.g. `display: none`) — Chrome surfaces this as a
//! `-32000 "Could not compute box model"` Cdp error rather than a stale-node
//! signal, so we map that specific error to `None` instead of bubbling.
//! `bounding_box_page` layers `window.scrollX` / `scrollY` on top of that
//! box to yield document-relative ([`PageBox`]) coordinates.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::element::Element;
use crate::error::{Result, ZendriverError};
use crate::query::actionability;
use crate::query::{BoundingBox, PageBox};

/// The smallest axis-aligned rectangle around `quad`, a `DOM.getBoxModel`
/// quad (`[x1, y1, x2, y2, x3, y3, x4, y4]`, its four corners), or `None`
/// when `quad` isn't exactly eight numbers.
fn border_box(quad: &Value) -> Option<BoundingBox> {
    let coords: Vec<f64> = quad
        .as_array()?
        .iter()
        .map(Value::as_f64)
        .collect::<Option<_>>()?;
    if coords.len() != 8 {
        return None;
    }
    let mut left = f64::INFINITY;
    let mut top = f64::INFINITY;
    let mut right = f64::NEG_INFINITY;
    let mut bottom = f64::NEG_INFINITY;
    for corner in coords.chunks_exact(2) {
        left = left.min(corner[0]);
        right = right.max(corner[0]);
        top = top.min(corner[1]);
        bottom = bottom.max(corner[1]);
    }
    Some(BoundingBox {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

impl Element {
    /// Return the value of attribute `name`, or `None` when absent.
    ///
    /// Routes through `el.getAttribute(name)`, run in zendriver's isolated
    /// world, so HTML attributes (`href`, `data-*`) and ARIA attributes read
    /// as they are in the DOM, whatever the page's JavaScript has done to
    /// `getAttribute`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let link = tab.find().css("a").one().await?;
    /// if let Some(href) = link.attr("href").await? {
    ///     println!("link goes to {href}");
    /// }
    /// # Ok(()) }
    /// ```
    pub async fn attr(&self, name: impl AsRef<str>) -> Result<Option<String>> {
        let name = name.as_ref().to_string();
        self.with_refresh(|| {
            let name = name.clone();
            async move {
                let res = self
                    .call_on(
                        "function(n){ return this.getAttribute(n); }",
                        json!([{ "value": name }]),
                    )
                    .await?;
                // `getAttribute` returns `null` for missing attributes →
                // `value` field is missing/null on the RemoteObject.
                match res.get("value") {
                    Some(Value::String(s)) => Ok(Some(s.clone())),
                    _ => Ok(None),
                }
            }
        })
        .await
    }

    /// Return a snapshot of every attribute on the element.
    ///
    /// Built from `el.attributes` (live `NamedNodeMap`) on the JS side,
    /// materialized into a plain object before crossing the CDP boundary
    /// so `returnByValue` can serialize it.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let el = tab.find().css("input").one().await?;
    /// for (k, v) in el.attrs().await? {
    ///     println!("{k}={v}");
    /// }
    /// # Ok(()) }
    /// ```
    pub async fn attrs(&self) -> Result<HashMap<String, String>> {
        self.with_refresh(|| async move {
            let js = r"
                function() {
                    const out = {};
                    for (const a of this.attributes) { out[a.name] = a.value; }
                    return out;
                }
            ";
            let res = self.call_on(js, json!([])).await?;
            let value = res.get("value").cloned().unwrap_or(Value::Null);
            let obj = value.as_object().cloned().unwrap_or_default();
            let mut out = HashMap::with_capacity(obj.len());
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    out.insert(k, s.to_string());
                }
            }
            Ok(out)
        })
        .await
    }

    /// Return the element's `innerText` (rendered text, excluding hidden
    /// subtrees).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let h1 = tab.find().css("h1").one().await?;
    /// assert_eq!(h1.inner_text().await?, "Example Domain");
    /// # Ok(()) }
    /// ```
    pub async fn inner_text(&self) -> Result<String> {
        self.with_refresh(|| async move {
            let res = self
                .call_on("function(){ return this.innerText; }", json!([]))
                .await?;
            Ok(res["value"].as_str().unwrap_or("").to_string())
        })
        .await
    }

    /// Return the element's `innerHTML` (serialized child markup).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let div = tab.find().css("div.content").one().await?;
    /// println!("{}", div.inner_html().await?);
    /// # Ok(()) }
    /// ```
    pub async fn inner_html(&self) -> Result<String> {
        self.with_refresh(|| async move {
            let res = self
                .call_on("function(){ return this.innerHTML; }", json!([]))
                .await?;
            Ok(res["value"].as_str().unwrap_or("").to_string())
        })
        .await
    }

    /// Return the element's `outerHTML` (serialized element + children).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let h1 = tab.find().css("h1").one().await?;
    /// assert!(h1.outer_html().await?.starts_with("<h1"));
    /// # Ok(()) }
    /// ```
    pub async fn outer_html(&self) -> Result<String> {
        self.with_refresh(|| async move {
            let res = self
                .call_on("function(){ return this.outerHTML; }", json!([]))
                .await?;
            Ok(res["value"].as_str().unwrap_or("").to_string())
        })
        .await
    }

    /// Return the viewport-relative bounding box, or `None` when absent.
    ///
    /// `None` typically means the element has no box (e.g. `display: none`)
    /// — Chrome reports this as `-32000 "Could not compute box model"` which
    /// is mapped to `None` rather than bubbling.
    ///
    /// The box is the element's border box: the smallest rectangle around
    /// the four corners of `DOM.getBoxModel`'s `border` quad, which a CSS
    /// transform can rotate. Its coordinates are those of the top-level
    /// page's viewport, so for a top-level element it is the box that
    /// `getBoundingClientRect()` reports, and for an element in an iframe,
    /// that box moved by the iframe's position. (`DOM.getBoxModel`, unlike
    /// `DOM.getContentQuads`, brings the page's layout up to date first, so
    /// the box is current even in a background tab, which renders no
    /// frames.)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let btn = tab.find().css("button").one().await?;
    /// if let Some(bbox) = btn.bounding_box().await? {
    ///     println!("button at ({}, {}) size {}x{}", bbox.x, bbox.y, bbox.width, bbox.height);
    /// }
    /// # Ok(()) }
    /// ```
    pub async fn bounding_box(&self) -> Result<Option<BoundingBox>> {
        self.with_refresh(|| async move {
            let backend_node_id = self.backend_node_id_cloned().await?;
            let res = self
                .inner
                .tab
                .call(
                    "DOM.getBoxModel",
                    json!({ "backendNodeId": backend_node_id }),
                )
                .await;
            let res = match res {
                Ok(v) => v,
                // No box (display: none, detached, etc.) → None.
                Err(ZendriverError::Cdp { ref message, .. })
                    if message.contains("Could not compute box model") =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            Ok(border_box(&res["model"]["border"]))
        })
        .await
    }

    /// Return the page-absolute bounding box, or `None` when absent.
    ///
    /// Combines the viewport-relative [`Element::bounding_box`] with the
    /// page scroll offset (`window.scrollX` / `scrollY`), so the resulting
    /// [`PageBox`] can yield document-relative coordinates that stay stable
    /// as the page scrolls — via [`PageBox::abs_origin`] (top-left) and
    /// [`PageBox::abs_center`] (center).
    ///
    /// Ports nodriver's `Position.abs_x` / `abs_y` (element.py:504), the
    /// element center offset by the scroll position. `None` has the same
    /// meaning as in [`Element::bounding_box`] — the node has no box.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let btn = tab.find().css("button").one().await?;
    /// if let Some(page) = btn.bounding_box_page().await? {
    ///     let (ax, ay) = page.abs_center();
    ///     println!("button center is at page ({ax}, {ay})");
    /// }
    /// # Ok(()) }
    /// ```
    pub async fn bounding_box_page(&self) -> Result<Option<PageBox>> {
        self.with_refresh(|| async move {
            let Some(viewport) = self.bounding_box().await? else {
                return Ok(None);
            };
            // One round-trip for both scroll offsets. `pageXOffset` /
            // `pageYOffset` are the spec-stable aliases of `scrollX` /
            // `scrollY`.
            let res = self
                .call_on_main(
                    "function(){ return { x: window.scrollX, y: window.scrollY }; }",
                    json!([]),
                )
                .await?;
            let scroll = res.get("value").cloned().unwrap_or(Value::Null);
            let scroll_x = scroll.get("x").and_then(Value::as_f64).unwrap_or(0.0);
            let scroll_y = scroll.get("y").and_then(Value::as_f64).unwrap_or(0.0);
            Ok(Some(PageBox {
                viewport,
                scroll_x,
                scroll_y,
            }))
        })
        .await
    }

    /// Returns `true` iff the element is rendered.
    ///
    /// Attached to the document, has a positive bounding box, and is not
    /// hidden via `display`, `visibility`, `content-visibility: hidden`, or
    /// an effective opacity under 1% (on it or an ancestor). Where the
    /// element sits doesn't matter: one below the fold, or placed off the
    /// page, is visible. The same check as
    /// [`crate::FindBuilder::visible_only`]. An action (click, type)
    /// additionally requires the element to be in the viewport after it
    /// has scrolled the element into view.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let modal = tab.find().css(".modal").one().await?;
    /// if !modal.is_visible().await? {
    ///     println!("modal is hidden");
    /// }
    /// # Ok(()) }
    /// ```
    pub async fn is_visible(&self) -> Result<bool> {
        self.with_refresh(|| async move {
            actionability::check_visible(self, actionability::Within::Anywhere).await
        })
        .await
    }

    /// Returns `true` iff the element is not disabled.
    ///
    /// The element is disabled when any of these holds:
    ///
    /// - It matches CSS `:disabled`. So a control inside a disabled
    ///   `<fieldset>` is disabled unless it is inside that fieldset's first
    ///   `<legend>`, and so is an `<option>` in a disabled `<optgroup>`.
    /// - An ancestor that is not a `<fieldset>` matches `:disabled`, as a
    ///   disabled `<button>` does for a `<span>` inside it.
    /// - Of the element and its ancestors, the nearest one whose
    ///   `aria-disabled` is `true` or `false` (in any mix of upper and
    ///   lower case) has `true`.
    /// - It is a custom element (its name has a `-`) with a `disabled`
    ///   attribute. `:disabled` ignores a custom element unless it is
    ///   form-associated, and the check runs in zendriver's isolated world,
    ///   which can't see a `disabled` property that the page's JavaScript
    ///   defines; component libraries commonly reflect it to the attribute.
    ///
    /// The ancestors are those in the element's own tree: the walk doesn't
    /// leave the shadow root that contains the element. An element for
    /// which none of these holds is enabled.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let submit = tab.find().css("button[type=submit]").one().await?;
    /// if submit.is_enabled().await? {
    ///     submit.click().await?;
    /// }
    /// # Ok(()) }
    /// ```
    pub async fn is_enabled(&self) -> Result<bool> {
        self.with_refresh(|| async move { actionability::check_enabled(self).await })
            .await
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::tab::Tab;
    use zendriver_transport::SessionHandle;
    use zendriver_transport::testing::MockConnection;

    #[tokio::test]
    async fn attr_returns_some_when_attribute_present() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess);
        let el = Element::from_jsret(tab, 1, "R1".to_string());

        let fut = tokio::spawn({
            let e = el.clone();
            async move { e.attr("href").await }
        });

        let id = mock.expect_cmd("Runtime.callFunctionOn").await;
        let sent = mock.last_sent();
        assert_eq!(sent["params"]["objectId"], "R1");
        assert!(
            sent["params"]["functionDeclaration"]
                .as_str()
                .unwrap()
                .contains("getAttribute")
        );
        assert_eq!(sent["params"]["arguments"][0]["value"], "href");
        mock.reply(
            id,
            json!({ "result": { "value": "/login", "type": "string" } }),
        )
        .await;

        let got = fut.await.unwrap().unwrap();
        assert_eq!(got, Some("/login".to_string()));
        conn.shutdown();
    }

    #[tokio::test]
    async fn attrs_returns_hashmap_of_all_attributes() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess);
        let el = Element::from_jsret(tab, 1, "R1".to_string());

        let fut = tokio::spawn({
            let e = el.clone();
            async move { e.attrs().await }
        });

        let id = mock.expect_cmd("Runtime.callFunctionOn").await;
        let sent = mock.last_sent();
        assert!(
            sent["params"]["functionDeclaration"]
                .as_str()
                .unwrap()
                .contains("attributes")
        );
        mock.reply(
            id,
            json!({
                "result": {
                    "value": { "id": "btn", "class": "primary", "data-x": "42" },
                    "type": "object"
                }
            }),
        )
        .await;

        let map = fut.await.unwrap().unwrap();
        assert_eq!(map.get("id").map(String::as_str), Some("btn"));
        assert_eq!(map.get("class").map(String::as_str), Some("primary"));
        assert_eq!(map.get("data-x").map(String::as_str), Some("42"));
        assert_eq!(map.len(), 3);
        conn.shutdown();
    }

    #[tokio::test]
    async fn bounding_box_is_the_border_quads_box() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess);
        let el = Element::from_jsret(tab, 42, "R1".to_string());

        let fut = tokio::spawn({
            let e = el.clone();
            async move { e.bounding_box().await }
        });

        let id = mock.expect_cmd("DOM.getBoxModel").await;
        let sent = mock.last_sent();
        assert_eq!(sent["params"]["backendNodeId"], 42);
        mock.reply(
            id,
            json!({
                "model": {
                    // Four nested boxes; the border box's corners are
                    // top-left (10,20), top-right (110,20), bottom-right
                    // (110,70) and bottom-left (10,70).
                    "content": [35.0, 30.0, 85.0, 30.0, 85.0, 60.0, 35.0, 60.0],
                    "padding": [25.0, 25.0, 95.0, 25.0, 95.0, 65.0, 25.0, 65.0],
                    "border":  [10.0, 20.0, 110.0, 20.0, 110.0, 70.0, 10.0, 70.0],
                    "margin":  [0.0, 10.0, 120.0, 10.0, 120.0, 80.0, 0.0, 80.0],
                    "width":  100,
                    "height": 50
                }
            }),
        )
        .await;

        let bbox = fut.await.unwrap().unwrap().expect("should be Some");
        assert!((bbox.x - 10.0).abs() < 1e-9);
        assert!((bbox.y - 20.0).abs() < 1e-9);
        assert!((bbox.width - 100.0).abs() < 1e-9);
        assert!((bbox.height - 50.0).abs() < 1e-9);
        conn.shutdown();
    }

    /// A rotated border quad gets the smallest upright box around it; a
    /// quad that isn't eight numbers gets none.
    #[test]
    fn border_box_bounds_all_four_corners() {
        let diamond = json!([60.0, 0.0, 120.0, 60.0, 60.0, 120.0, 0.0, 60.0]);
        let expected = BoundingBox {
            x: 0.0,
            y: 0.0,
            width: 120.0,
            height: 120.0,
        };
        assert_eq!(border_box(&diamond), Some(expected));
        assert_eq!(border_box(&json!([1.0, 2.0, 3.0])), None);
        let stray = json!([60.0, 0.0, 120.0, 60.0, 60.0, 120.0, 0.0, 60.0, "x"]);
        assert_eq!(border_box(&stray), None);
        assert_eq!(border_box(&Value::Null), None);
    }

    #[tokio::test]
    async fn bounding_box_page_adds_scroll_offsets_to_viewport_box() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess);
        let el = Element::from_jsret(tab, 42, "R1".to_string());

        let fut = tokio::spawn({
            let e = el.clone();
            async move { e.bounding_box_page().await }
        });

        // Step 1: bounding_box → DOM.getBoxModel. Box top-left (10,20), 100x50.
        let id = mock.expect_cmd("DOM.getBoxModel").await;
        assert_eq!(mock.last_sent()["params"]["backendNodeId"], 42);
        mock.reply(
            id,
            json!({
                "model": {
                    "content": [10.0, 20.0, 110.0, 20.0, 110.0, 70.0, 10.0, 70.0],
                    "padding": [10.0, 20.0, 110.0, 20.0, 110.0, 70.0, 10.0, 70.0],
                    "border":  [10.0, 20.0, 110.0, 20.0, 110.0, 70.0, 10.0, 70.0],
                    "margin":  [10.0, 20.0, 110.0, 20.0, 110.0, 70.0, 10.0, 70.0],
                    "width":  100,
                    "height": 50
                }
            }),
        )
        .await;

        // Step 2: scroll offsets via Runtime.callFunctionOn (window.scrollX/Y).
        let id = mock.expect_cmd("Runtime.callFunctionOn").await;
        assert!(
            mock.last_sent()["params"]["functionDeclaration"]
                .as_str()
                .unwrap()
                .contains("scrollX")
        );
        mock.reply(
            id,
            json!({ "result": { "value": { "x": 1000.0, "y": 500.0 }, "type": "object" } }),
        )
        .await;

        let page = fut.await.unwrap().unwrap().expect("should be Some");
        // Viewport box is unchanged.
        assert!((page.viewport.x - 10.0).abs() < 1e-9);
        assert!((page.viewport.y - 20.0).abs() < 1e-9);
        assert!((page.scroll_x - 1000.0).abs() < 1e-9);
        assert!((page.scroll_y - 500.0).abs() < 1e-9);
        // Page-absolute origin = viewport origin + scroll.
        let (ax, ay) = page.abs_origin();
        assert!((ax - 1010.0).abs() < 1e-9);
        assert!((ay - 520.0).abs() < 1e-9);
        // Page-absolute center = origin + half-size + scroll (nodriver abs_x/y).
        let (cx, cy) = page.abs_center();
        assert!((cx - 1060.0).abs() < 1e-9); // 10 + 50 + 1000
        assert!((cy - 545.0).abs() < 1e-9); // 20 + 25 + 500
        conn.shutdown();
    }
}
