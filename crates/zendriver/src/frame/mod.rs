//! Handle to a single document frame within a [`crate::Tab`].
//!
//! A [`Frame`] wraps the CDP `frameId` plus the [`zendriver_transport::SessionHandle`]
//! that should be used to dispatch commands against that frame. For the
//! main frame the session is the owning tab's session (same-process); for
//! out-of-process iframes (OOPIFs) it's a distinct child session attached
//! via `Target.attachedToTarget`.
//!
//! ```no_run
//! # async fn ex() -> zendriver::Result<()> {
//! # let browser = zendriver::Browser::builder().launch().await?;
//! # let tab = browser.main_tab();
//! tab.goto("https://example.com").await?;
//! let main = tab.main_frame().await?;
//! let title: String = main.evaluate_main("document.title").await?;
//! # let _ = title;
//! # Ok(()) }
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use zendriver_transport::SessionHandle;

use crate::error::{Result, ZendriverError};
use crate::isolated_world::retry_once_on_stale_context;
use crate::load_tracker::next_stop;
use crate::tab::{Tab, TabInner};

pub mod lifecycle;
pub mod oopif;

/// Default wait window for [`Frame::wait_for_load`]. Mirrors the constant
/// in [`crate::tab::Tab::wait_for_load`] so single-frame and tab-level
/// navigation share their stall budget.
const DEFAULT_LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Cheap-to-clone handle to a single document frame.
///
/// Construct via [`crate::tab::Tab::main_frame`] (top-level frame for a
/// tab); sub-frames and OOPIFs are registered by the [`lifecycle`] and
/// [`oopif`] wiring and read back via [`crate::tab::Tab::frames`]. All
/// accessor methods operate on the inner `Arc`, so cloning a `Frame` is a
/// single refcount bump — and every clone of one entry observes the same
/// [`Frame::url`], since the lifecycle subscriber writes through that
/// shared `Arc`.
#[derive(Clone, Debug)]
pub struct Frame {
    inner: Arc<FrameInner>,
}

#[derive(Debug)]
pub(crate) struct FrameInner {
    /// CDP `frameId` (e.g. `"F0"`, a hex string at runtime). Stable for the
    /// lifetime of the frame.
    pub(crate) frame_id: String,
    /// Parent frame's CDP `frameId`. `None` for the main (top-level) frame;
    /// `Some` for every sub-frame and OOPIF.
    pub(crate) parent_frame_id: Option<String>,
    /// Last-known document URL for this frame. Behind an [`RwLock`] because
    /// the lifecycle subscriber mutates it on `Page.frameNavigated` while
    /// readers concurrently call [`Frame::url`].
    pub(crate) url: RwLock<String>,
    /// `<frame name>` / `<iframe name>` attribute if present.
    ///
    /// Set from `Page.frameNavigated`'s `frame.name` whenever Chrome
    /// supplies a non-empty one. `Page.frameAttached` carries no name, so
    /// an entry created from an attach starts `None` and is backfilled on
    /// the first navigation that reports one; an event that simply omits
    /// the field never clears a name already established.
    ///
    /// Immutable, which is what makes that backfill a *replacement* of the
    /// registry entry rather than a write in place — see
    /// [`lifecycle::run`]. Interior mutability here would make it a write,
    /// at the cost of turning [`Frame::name`] into an `async fn`.
    pub(crate) name: Option<String>,
    /// CDP session used to dispatch commands against this frame. The main
    /// frame shares the owning tab's session; OOPIFs attach to a distinct
    /// child session whose handle is plumbed in at construction.
    pub(crate) session: SessionHandle,
    /// Weak ref to the owning tab. Upgraded by
    /// [`Frame::tab_for_synthesize`] when the query layer needs to wrap
    /// a `RemoteRef` in an `Element` (the `Element` stores an owned
    /// `Tab` clone for the lifetime of the handle). `Weak` so that a
    /// long-held `Frame` clone does not pin the tab alive past its
    /// public lifetime.
    pub(crate) tab: Weak<TabInner>,
}

impl Frame {
    /// Construct a `Frame` from its CDP identity + the session that should
    /// dispatch commands against it.
    ///
    /// Called by [`crate::tab::Tab::main_frame`] (main-frame path, shares
    /// the tab's session), by the [`lifecycle`] subscriber (sub-frame
    /// attach and name backfill) and by the [`oopif`] attach observer
    /// (distinct child session).
    pub(crate) fn new(
        frame_id: String,
        parent_frame_id: Option<String>,
        url: String,
        name: Option<String>,
        session: SessionHandle,
        tab: Weak<TabInner>,
    ) -> Self {
        Self {
            inner: Arc::new(FrameInner {
                frame_id,
                parent_frame_id,
                url: RwLock::new(url),
                name,
                session,
                tab,
            }),
        }
    }

    /// The frame's CDP `frameId`. Chrome keeps it for the frame's whole life
    /// (it is the frame tree node's DevTools token, which a move to another
    /// process keeps), so once Chrome rejects it with "No frame for given id
    /// found", the frame is gone from this tab's session: it was removed, or
    /// it moved to another renderer process (an out-of-process iframe).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let main = tab.main_frame().await?;
    /// let _: &str = main.id();
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn id(&self) -> &str {
        &self.inner.frame_id
    }

    /// CDP session used to dispatch commands against this frame. Crate-
    /// internal — exposed so `query::selectors` can route
    /// `Runtime.evaluate` / `DOM.describeNode` etc. through the right
    /// session for [`crate::query::FindBuilder::new_for_frame`] queries.
    ///
    /// For the main frame and same-origin sub-frames this is identical
    /// to the parent tab's session; for out-of-process iframes it is a
    /// distinct child session attached via `Target.attachedToTarget`.
    pub(crate) fn session(&self) -> &SessionHandle {
        &self.inner.session
    }

    /// Upgrade the frame's `Weak<TabInner>` into an owned `Tab` for
    /// [`crate::element::Element::synthesize_query`] callers. Returns
    /// `None` if the owning Tab was dropped — in practice this should
    /// not happen because Frames are constructed by Tabs that hold a
    /// strong reference, but the contract is exposed honestly here so
    /// the caller (the `QueryScope::synthesize_tab` accessor in
    /// `query::selectors`) can decide between erroring and panicking.
    pub(crate) fn tab_for_synthesize(&self) -> Option<Tab> {
        self.inner.tab.upgrade().map(|inner| Tab { inner })
    }

    /// The frame's parent CDP `frameId`.
    ///
    /// `None` iff this is the main (top-level) frame for the owning tab.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// for f in tab.frames().await? {
    ///     if let Some(parent) = f.parent_id() {
    ///         println!("frame {} is a child of {parent}", f.id());
    ///     }
    /// }
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn parent_id(&self) -> Option<&str> {
        self.inner.parent_frame_id.as_deref()
    }

    /// The frame's `name` attribute (`<iframe name="...">`).
    ///
    /// `None` for frames without an explicit name (including the main frame
    /// in most cases).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// for f in tab.frames().await? {
    ///     println!("name: {:?}", f.name());
    /// }
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.inner.name.as_deref()
    }

    /// `true` iff this is the main (top-level) frame for its owning tab.
    ///
    /// Equivalent to `parent_id().is_none()`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let main = tab.main_frame().await?;
    /// assert!(main.is_main());
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn is_main(&self) -> bool {
        self.inner.parent_frame_id.is_none()
    }

    /// The frame's current document URL.
    ///
    /// Snapshot under an `RwLock`; kept fresh by the frame lifecycle
    /// subscriber as `Page.frameNavigated` events arrive.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let main = tab.main_frame().await?;
    /// println!("{}", main.url().await);
    /// # Ok(()) }
    /// ```
    pub async fn url(&self) -> String {
        self.inner.url.read().await.clone()
    }

    /// Evaluate JS in an isolated world bound to **this frame**.
    ///
    /// Sandboxed — no page globals visible. Result deserialized into `T`.
    /// [`crate::Tab::evaluate`] is this on the tab's main frame.
    ///
    /// Each call first asks Chrome for the id of the isolated world's
    /// context in the frame's current document, so a `Frame` handle kept
    /// across a navigation of its frame keeps working. If Chrome answers the
    /// evaluation with `Cannot find context with specified id`, because a
    /// navigation destroyed that context after the lookup, the call is
    /// retried once. A navigation into another renderer process in that
    /// interval can instead hand the id to a context of the new process,
    /// and the expression then runs there. A navigation while the
    /// expression runs fails the call, with no retry, since the expression
    /// may already have had effects.
    ///
    /// # Errors
    ///
    /// Returns [`ZendriverError::JsException`] when the expression raises;
    /// [`ZendriverError::Serde`] when the result cannot be decoded.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let frame = tab.main_frame().await?;
    /// let url: String = frame.evaluate("location.href").await?;
    /// # let _ = url;
    /// # Ok(()) }
    /// ```
    pub async fn evaluate<T: DeserializeOwned>(&self, js: impl AsRef<str>) -> Result<T> {
        let js = js.as_ref();
        let session = &self.inner.session;
        let res = retry_once_on_stale_context(|| async move {
            let ctx_id = self.ensure_isolated_world().await?;
            let params = json!({
                "expression": js,
                "contextId": ctx_id,
                "returnByValue": true,
                "awaitPromise": true,
            });
            Ok(session.call("Runtime.evaluate", params).await?)
        })
        .await?;
        Self::extract_value(&res)
    }

    /// Whether this frame is the top frame of its CDP session, where
    /// `Runtime.evaluate` with no `contextId` runs in the frame's main
    /// world: the tab's main frame, or an OOPIF, which zendriver registers
    /// with a session of its own (and registers no frame under it there).
    fn is_session_root(&self) -> bool {
        if self.is_main() {
            return true;
        }
        let Some(tab) = self.tab_for_synthesize() else {
            return false;
        };
        tab.session().session_id() != self.inner.session.session_id()
    }

    /// The `executionContextId` of the main world of this frame's current
    /// document, found without `Runtime.enable` (which pages can detect).
    ///
    /// The frame's isolated world (see [`Frame::ensure_isolated_world`],
    /// which also recovers a frame id that Chrome has replaced) gives the
    /// frame's `document`. `DOM.resolveNode` with no context id wraps that
    /// document in the main world of the document's own frame, and V8
    /// writes that world's context id into the id of the object it returns
    /// (see [`crate::isolated_world::context_id_of_object`]). The lookup's
    /// objects are in an object group of their own, which this releases
    /// before it returns, best effort.
    ///
    /// Asks Chrome on every call, for the reason given on
    /// [`Frame::ensure_isolated_world`].
    ///
    /// # Errors
    ///
    /// The error of any of the calls; [`ZendriverError::JsException`] if
    /// reading `document` throws; or [`ZendriverError::Navigation`] when a
    /// reply lacks what it should hold, and (with a message that
    /// [`crate::isolated_world::is_stale_context`] recognizes) when the node
    /// that `DOM.resolveNode` wraps is no document, as after a navigation
    /// into another renderer process gave its backend node id to another
    /// node.
    async fn main_world_context_id(&self) -> Result<i64> {
        static NEXT_LOOKUP: AtomicU64 = AtomicU64::new(0);
        let session = &self.inner.session;
        let group = format!(
            "zendriver-main-world-lookup-{}",
            NEXT_LOOKUP.fetch_add(1, Ordering::Relaxed)
        );
        let found: Result<i64> = async {
            let isolated = self.ensure_isolated_world().await?;
            let missing = |what: &str| ZendriverError::Navigation(format!("frame {}: {what}", self.id()));
            let reply = session
                .call(
                    "Runtime.evaluate",
                    json!({ "expression": "document", "contextId": isolated, "objectGroup": group }),
                )
                .await?;
            if let Some(e) = ZendriverError::js_exception_in(&reply) {
                return Err(e);
            }
            let isolated_document = reply["result"]["objectId"]
                .as_str()
                .ok_or_else(|| missing("no object for its document"))?;
            let described = session
                .call("DOM.describeNode", json!({ "objectId": isolated_document }))
                .await?;
            let backend_node_id = described["node"]["backendNodeId"].as_i64().ok_or_else(|| {
                missing("DOM.describeNode returned no backendNodeId for its document")
            })?;
            let resolved = session
                .call(
                    "DOM.resolveNode",
                    json!({ "backendNodeId": backend_node_id, "objectGroup": group }),
                )
                .await?;
            let main_document = &resolved["object"];
            if main_document["description"] != "#document" {
                return Err(ZendriverError::Navigation(format!(
                    "Cannot find context of frame {}: its document's backend node id now names \
                     another node",
                    self.id()
                )));
            }
            let main_document = main_document["objectId"]
                .as_str()
                .ok_or_else(|| missing("DOM.resolveNode returned no objectId for its document"))?;
            crate::isolated_world::context_id_of_object(main_document)
        }
        .await;
        // Best effort: a handle left behind only holds memory until its
        // document goes away.
        let _ = session
            .call(
                "Runtime.releaseObjectGroup",
                json!({ "objectGroup": group }),
            )
            .await;
        found
    }

    /// Evaluate JS in this frame's **main world** (page globals visible).
    ///
    /// Escape hatch when isolated-world semantics don't fit. Dispatches
    /// `Runtime.evaluate` on the frame's session. For the top frame of that
    /// session (the tab's main frame, or an OOPIF on a session of its own)
    /// it sends no `contextId`; for any other iframe it sends the id of the
    /// iframe's main world, which it looks up first without
    /// `Runtime.enable`. `js` is a script, as for
    /// [`crate::Tab::evaluate_main`]: its completion value (for a script
    /// that ends with an expression, that expression's value) is the
    /// result.
    ///
    /// On such an iframe, a navigation can destroy a context that the call
    /// looked up before the call uses it; the call then looks its contexts
    /// up again once and retries, as [`Frame::evaluate`] does.
    ///
    /// # Errors
    ///
    /// [`ZendriverError::JsException`] when the script throws, and
    /// [`ZendriverError::Serde`] when the result can't be decoded into `T`.
    /// Otherwise the error of a CDP call (such as [`ZendriverError::Cdp`] or
    /// [`ZendriverError::CdpTimeout`]): of the evaluation, or of the lookup,
    /// for example when the iframe is gone (a
    /// [`ZendriverError::FrameNotFound`] when its id can't be recovered).
    /// The lookup can also fail with [`ZendriverError::Navigation`] when a
    /// reply lacks what it should hold.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let frame = tab.main_frame().await?;
    /// let title: String = frame.evaluate_main("document.title").await?;
    /// # let _ = title;
    /// # Ok(()) }
    /// ```
    pub async fn evaluate_main<T: DeserializeOwned>(&self, js: impl AsRef<str>) -> Result<T> {
        let js = js.as_ref();
        let session = &self.inner.session;
        let res = if self.is_session_root() {
            let params = json!({ "expression": js, "returnByValue": true, "awaitPromise": true });
            session.call("Runtime.evaluate", params).await?
        } else {
            retry_once_on_stale_context(|| async move {
                let context_id = self.main_world_context_id().await?;
                let params = json!({
                    "expression": js,
                    "contextId": context_id,
                    "returnByValue": true,
                    "awaitPromise": true,
                });
                Ok(session.call("Runtime.evaluate", params).await?)
            })
            .await?
        };
        Self::extract_value(&res)
    }

    /// The frame's current document HTML.
    ///
    /// Returns the serialized `document.documentElement.outerHTML`. Backed
    /// by [`Frame::evaluate_main`], so for an iframe this is the iframe's
    /// *own* HTML, not the parent page's.
    ///
    /// # Errors
    ///
    /// Those of [`Frame::evaluate_main`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let frame = tab.main_frame().await?;
    /// let html = frame.content().await?;
    /// assert!(html.contains("<html"));
    /// # Ok(()) }
    /// ```
    pub async fn content(&self) -> Result<String> {
        self.evaluate_main("document.documentElement.outerHTML")
            .await
    }

    /// The tab this frame belongs to.
    ///
    /// # Errors
    ///
    /// [`ZendriverError::Navigation`] if every handle to the tab was dropped.
    fn owning_tab(&self) -> Result<Tab> {
        self.tab_for_synthesize()
            .ok_or_else(|| ZendriverError::Navigation("the frame's tab is gone".into()))
    }

    /// Navigate **this frame** to `url`.
    ///
    /// Only supported for the main frame, where it is [`crate::Tab::goto`]
    /// — sub-frame navigation must be driven by mutating the parent
    /// document's `<iframe src>`. Does NOT wait for the load to complete;
    /// pair with [`Frame::wait_for_load`].
    ///
    /// # Errors
    ///
    /// Returns [`ZendriverError::Navigation`] when called on a sub-frame,
    /// when the owning tab is gone, or when Chrome reports `errorText` on
    /// the response.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let main = tab.main_frame().await?;
    /// main.goto("https://example.org").await?;
    /// main.wait_for_load().await?;
    /// # Ok(()) }
    /// ```
    pub async fn goto(&self, url: impl AsRef<str>) -> Result<()> {
        if !self.is_main() {
            return Err(ZendriverError::Navigation(
                "sub-frame goto not supported; set iframe.src via parent evaluate_main".into(),
            ));
        }
        self.owning_tab()?.goto(url).await
    }

    /// Wait until **this frame** has finished loading.
    ///
    /// For the main frame this is [`crate::Tab::wait_for_load`]. For a
    /// sub-frame it waits for the frame's next `Page.frameStoppedLoading`,
    /// so call it before that frame finishes loading. Default timeout
    /// matches [`crate::Tab::wait_for_load`] (30s).
    ///
    /// # Errors
    ///
    /// Returns [`ZendriverError::Timeout`] on expiry, and
    /// [`ZendriverError::Navigation`] if the tab is closed, or its
    /// connection closes, first. For a sub-frame in its own process, it
    /// also returns `Navigation` if the frame's session ends first (as when
    /// the frame is removed, or moves into its parent's process).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let main = tab.main_frame().await?;
    /// main.goto("https://example.com").await?;
    /// main.wait_for_load().await?;
    /// # Ok(()) }
    /// ```
    pub async fn wait_for_load(&self) -> Result<()> {
        if self.is_main() {
            return self.owning_tab()?.wait_for_load().await;
        }
        next_stop(
            &self.inner.session,
            &self.inner.frame_id,
            DEFAULT_LOAD_TIMEOUT,
        )
        .await
    }

    /// Begin a chainable element query against this frame's document.
    ///
    /// Mirrors [`crate::Tab::find`]. Queries dispatch on **this frame's**
    /// CDP session; the query root is the frame's own `document` (matches
    /// in sibling frames or the parent document are not considered).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let frame = tab.main_frame().await?;
    /// let h1 = frame.find().css("h1").one().await?;
    /// # let _ = h1;
    /// # Ok(()) }
    /// ```
    pub fn find(&self) -> crate::query::FindBuilder<'_> {
        crate::query::FindBuilder::new_for_frame(self)
    }

    /// Begin a chainable element query against this frame's document that
    /// returns ALL matches.
    ///
    /// Mirror of [`Frame::find`] (no `.nth`); terminate with `.many()` or
    /// `.many_or_empty()`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let frame = tab.main_frame().await?;
    /// let links = frame.find_all().css("a").many_or_empty().await?;
    /// # let _ = links;
    /// # Ok(()) }
    /// ```
    pub fn find_all(&self) -> crate::query::FindAllBuilder<'_> {
        crate::query::FindAllBuilder::new_for_frame(self)
    }

    /// Find one element by CSS selector. Python-parity convenience for
    /// `find().css(sel).one()`. For modifiers use the builder directly.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::ZendriverError::ElementNotFound`] if no element
    /// matches, and [`crate::error::ZendriverError::JsException`] at once if
    /// `css` is invalid.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let frame = tab.main_frame().await?;
    /// let h1 = frame.select("h1").await?;
    /// # let _ = h1;
    /// # Ok(()) }
    /// ```
    pub async fn select(&self, css: &str) -> crate::error::Result<crate::Element> {
        self.find().css(css).one().await
    }

    /// Find all elements by CSS selector. Python-parity convenience for
    /// `find_all().css(sel).many()`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::ZendriverError::ElementNotFound`] if no elements
    /// match, and [`crate::error::ZendriverError::JsException`] at once if
    /// `css` is invalid.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let frame = tab.main_frame().await?;
    /// let links = frame.select_all("a").await?;
    /// # let _ = links;
    /// # Ok(()) }
    /// ```
    pub async fn select_all(&self, css: &str) -> crate::error::Result<Vec<crate::Element>> {
        self.find_all().css(css).many().await
    }

    /// The `executionContextId` of zendriver's isolated world in this
    /// frame's current document, creating the world if needed.
    ///
    /// Asks Chrome on every call (with `Page.createIsolatedWorld`), because a
    /// remembered id can name a different context: a cross-process
    /// navigation numbers the new process's contexts from 1 again, so an id
    /// from the previous document can belong to, say, an iframe of the new
    /// one, and a call with it then runs there without any error. Chrome
    /// keeps one isolated world per name, with one context in each
    /// document, so a repeated call on the same document returns the same
    /// context and creates nothing. The returned id can still come to name
    /// another context in the same way, if such a navigation commits before
    /// Chrome handles the caller's command that uses the id.
    ///
    /// # Errors
    ///
    /// [`ZendriverError::FrameNotFound`] when Chrome rejects the frame's id:
    /// the frame is gone from this tab's session (see [`Frame::id`]).
    pub(crate) async fn ensure_isolated_world(&self) -> Result<i64> {
        match self.create_isolated_world(&self.inner.frame_id).await {
            Err(ZendriverError::Cdp {
                code: -32602,
                ref message,
                ..
            }) if message.contains("No frame for given id") => {
                Err(ZendriverError::FrameNotFound(format!(
                    "frame {} is gone from this tab's session (removed, or moved to another process)",
                    self.inner.frame_id
                )))
            }
            other => other,
        }
    }

    async fn create_isolated_world(&self, frame_id: &str) -> Result<i64> {
        let res = self
            .inner
            .session
            .call(
                "Page.createIsolatedWorld",
                json!({
                    "frameId": frame_id,
                    "worldName": "zendriver-eval",
                    "grantUniversalAccess": false,
                }),
            )
            .await?;
        res["executionContextId"].as_i64().ok_or_else(|| {
            ZendriverError::Navigation(
                "Page.createIsolatedWorld did not return executionContextId".into(),
            )
        })
    }

    /// Shared post-processing for `Runtime.evaluate` responses — checks
    /// `exceptionDetails` first (raising [`ZendriverError::JsException`])
    /// then deserializes `result.value` into `T`. Used by
    /// [`Frame::evaluate`], and so by [`crate::tab::Tab::evaluate`];
    /// [`crate::tab::Tab::evaluate_main`] has the same steps inline.
    #[allow(clippy::result_large_err)] // ZendriverError variance is the project-wide return type
    fn extract_value<T: DeserializeOwned>(res: &Value) -> Result<T> {
        if let Some(e) = ZendriverError::js_exception_in(res) {
            return Err(e);
        }
        let value = res
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null);
        serde_json::from_value(value).map_err(ZendriverError::Serde)
    }
}

impl crate::traits::Queryable for Frame {
    fn find(&self) -> crate::query::FindBuilder<'_> {
        Frame::find(self)
    }
    fn find_all(&self) -> crate::query::FindAllBuilder<'_> {
        Frame::find_all(self)
    }
}

#[async_trait::async_trait]
impl crate::traits::Evaluable for Frame {
    async fn evaluate<T>(&self, js: &str) -> Result<T>
    where
        T: DeserializeOwned + Send + 'static,
    {
        Frame::evaluate(self, js).await
    }
    async fn evaluate_main<T>(&self, js: &str) -> Result<T>
    where
        T: DeserializeOwned + Send + 'static,
    {
        Frame::evaluate_main(self, js).await
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use zendriver_transport::testing::MockConnection;

    /// Wait for the driver's next `method` command and return its id.
    ///
    /// Always used in place of `MockConnection::expect_cmd` directly:
    /// that method has no timeout of its own and silently discards frames
    /// that do not match, so a command the driver never sends hangs the
    /// test forever instead of failing it. A regression that removes a
    /// round-trip should turn CI red, not wedge it.
    async fn next_cmd(mock: &mut MockConnection, method: &str) -> u64 {
        tokio::time::timeout(Duration::from_secs(2), mock.expect_cmd(method))
            .await
            .unwrap_or_else(|_| panic!("driver never sent {method} within 2s"))
    }

    /// Build a synthetic `Frame` whose session sits on the supplied mock
    /// connection. Mirrors `Tab::new_for_test` ergonomics — no parent tab,
    /// no parent frame, fixed frameId / url, ready for evaluate dispatch.
    fn frame_on(session: SessionHandle, frame_id: &str) -> Frame {
        Frame::new(
            frame_id.to_string(),
            None,
            String::new(),
            None,
            session,
            Weak::new(),
        )
    }

    /// Every `Frame::evaluate` call dispatches `Page.createIsolatedWorld`
    /// with the frame's own `frameId`, and evaluates in the context it
    /// returns. A context id kept from an earlier call could name a context
    /// of another document.
    #[tokio::test]
    async fn evaluate_looks_up_the_isolated_world_on_every_call() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = frame_on(sess, "FRAME_A");

        // --- Call 1: full handshake + eval. ---
        let fut1 = tokio::spawn({
            let f = frame.clone();
            async move { f.evaluate::<i32>("1").await }
        });
        let id_world = next_cmd(&mut mock, "Page.createIsolatedWorld").await;
        assert_eq!(mock.last_sent()["params"]["frameId"], "FRAME_A");
        assert_eq!(mock.last_sent()["params"]["worldName"], "zendriver-eval");
        mock.reply(id_world, json!({ "executionContextId": 7 }))
            .await;
        let id_eval1 = next_cmd(&mut mock, "Runtime.evaluate").await;
        assert_eq!(mock.last_sent()["params"]["contextId"], 7);
        assert_eq!(mock.last_sent()["params"]["expression"], "1");
        mock.reply(
            id_eval1,
            json!({ "result": { "value": 1, "type": "number" } }),
        )
        .await;
        assert_eq!(fut1.await.unwrap().unwrap(), 1);

        // --- Call 2: the same handshake again, then the context it
        //     returned.
        let fut2 = tokio::spawn({
            let f = frame.clone();
            async move { f.evaluate::<i32>("2").await }
        });
        let id_world2 = next_cmd(&mut mock, "Page.createIsolatedWorld").await;
        assert_eq!(mock.last_sent()["params"]["frameId"], "FRAME_A");
        mock.reply(id_world2, json!({ "executionContextId": 8 }))
            .await;
        let id_eval2 = next_cmd(&mut mock, "Runtime.evaluate").await;
        assert_eq!(mock.last_sent()["params"]["contextId"], 8);
        assert_eq!(mock.last_sent()["params"]["expression"], "2");
        mock.reply(
            id_eval2,
            json!({ "result": { "value": 2, "type": "number" } }),
        )
        .await;
        assert_eq!(fut2.await.unwrap().unwrap(), 2);

        conn.shutdown();
    }

    /// A navigation inside the frame can destroy the context between
    /// `Page.createIsolatedWorld` and `Runtime.evaluate`. `Frame::evaluate`
    /// must then look the world up again and retry once, transparently.
    #[tokio::test]
    async fn evaluate_retries_in_a_fresh_context_after_a_stale_one() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = frame_on(sess, "FRAME_C");

        let fut = tokio::spawn({
            let f = frame.clone();
            async move { f.evaluate::<i32>("1").await }
        });

        let id_world = next_cmd(&mut mock, "Page.createIsolatedWorld").await;
        mock.reply(id_world, json!({ "executionContextId": 7 }))
            .await;
        let id_eval = next_cmd(&mut mock, "Runtime.evaluate").await;
        assert_eq!(mock.last_sent()["params"]["contextId"], 7);
        // Chrome's answer once the context behind id 7 is gone.
        mock.reply_err(id_eval, -32000, "Cannot find context with specified id")
            .await;

        // The retry: the world's context in the new document, then the
        // same expression again.
        let id_world2 = next_cmd(&mut mock, "Page.createIsolatedWorld").await;
        mock.reply(id_world2, json!({ "executionContextId": 9 }))
            .await;
        let id_eval2 = next_cmd(&mut mock, "Runtime.evaluate").await;
        assert_eq!(mock.last_sent()["params"]["contextId"], 9);
        assert_eq!(mock.last_sent()["params"]["expression"], "1");
        mock.reply(
            id_eval2,
            json!({ "result": { "value": 1, "type": "number" } }),
        )
        .await;

        assert_eq!(fut.await.unwrap().unwrap(), 1);
        conn.shutdown();
    }

    /// The retry is bounded at one. A context that is stale again on the
    /// second attempt surfaces the error instead of looping.
    #[tokio::test]
    async fn evaluate_retries_a_stale_context_only_once() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = frame_on(sess, "FRAME_D");

        let fut = tokio::spawn({
            let f = frame.clone();
            async move { f.evaluate::<i32>("1").await }
        });

        for ctx in [7, 9] {
            let id_world = next_cmd(&mut mock, "Page.createIsolatedWorld").await;
            mock.reply(id_world, json!({ "executionContextId": ctx }))
                .await;
            let id_eval = next_cmd(&mut mock, "Runtime.evaluate").await;
            mock.reply_err(id_eval, -32000, "Cannot find context with specified id")
                .await;
        }

        let err = fut.await.unwrap().expect_err("second stale must surface");
        assert!(
            matches!(err, ZendriverError::Navigation(ref m) if m.contains("Cannot find context")),
            "unexpected error: {err:?}",
        );
        conn.shutdown();
    }

    /// On the tab's main frame (no parent), `Frame::evaluate_main`
    /// dispatches `Runtime.evaluate` directly on the frame's session with
    /// NO `contextId` parameter — proving that this path skips the
    /// isolated-world handshake entirely (no `Page.createIsolatedWorld` in
    /// the trace).
    #[tokio::test]
    async fn evaluate_main_on_the_main_frame_sends_no_context_id() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = frame_on(sess, "FRAME_B");

        let fut = tokio::spawn({
            let f = frame.clone();
            async move { f.evaluate_main::<i32>("1+1").await }
        });

        // Next outbound frame is Runtime.evaluate itself — no isolated-world
        // bootstrap. `last_sent()` after `expect_cmd` confirms there is no
        // `contextId` field on the params.
        let id = next_cmd(&mut mock, "Runtime.evaluate").await;
        assert_eq!(mock.last_sent()["params"]["expression"], "1+1");
        assert!(mock.last_sent()["params"].get("contextId").is_none());
        mock.reply(id, json!({ "result": { "value": 2, "type": "number" } }))
            .await;

        assert_eq!(fut.await.unwrap().unwrap(), 2);
        conn.shutdown();
    }

    /// A `Frame` for the iframe `CHILD` under `TOP`, on `session`, with no
    /// tab.
    fn iframe_on(session: SessionHandle) -> Frame {
        Frame::new(
            "CHILD".to_string(),
            Some("TOP".to_string()),
            String::new(),
            None,
            session,
            Weak::new(),
        )
    }

    /// Serve the main-world lookup of [`Frame::main_world_context_id`] for
    /// the frame `CHILD`, answering that the main world's context id is
    /// `context`, and return the lookup's object group.
    async fn serve_main_world_lookup(mock: &mut MockConnection, context: i64) -> String {
        let id = next_cmd(mock, "Page.createIsolatedWorld").await;
        assert_eq!(mock.last_sent()["params"]["frameId"], "CHILD");
        mock.reply(id, json!({ "executionContextId": 7 })).await;
        let id = next_cmd(mock, "Runtime.evaluate").await;
        let params = mock.last_sent()["params"].clone();
        assert_eq!(params["expression"], "document");
        assert_eq!(params["contextId"], 7);
        let group = params["objectGroup"].as_str().unwrap().to_string();
        mock.reply(id, json!({ "result": { "objectId": "1.7.1" } }))
            .await;
        let id = next_cmd(mock, "DOM.describeNode").await;
        assert_eq!(mock.last_sent()["params"]["objectId"], "1.7.1");
        mock.reply(id, json!({ "node": { "backendNodeId": 40 } }))
            .await;
        let id = next_cmd(mock, "DOM.resolveNode").await;
        let params = mock.last_sent()["params"].clone();
        assert_eq!(params["backendNodeId"], 40);
        assert!(params.get("executionContextId").is_none());
        assert_eq!(params["objectGroup"], group.as_str());
        let object = json!({ "objectId": format!("1.{context}.2"), "description": "#document" });
        mock.reply(id, json!({ "object": object })).await;
        let id = next_cmd(mock, "Runtime.releaseObjectGroup").await;
        assert_eq!(mock.last_sent()["params"]["objectGroup"], group.as_str());
        mock.reply(id, json!({})).await;
        group
    }

    /// On an iframe, `Frame::evaluate_main` finds the iframe's main world
    /// through its isolated world: `document` there, its backend node id,
    /// `DOM.resolveNode` with no context id, and the context id in the id
    /// of the object that comes back. It releases the lookup's objects,
    /// then evaluates there.
    #[tokio::test]
    async fn evaluate_main_on_an_iframe_evaluates_in_its_main_world() {
        let (mut mock, conn) = MockConnection::pair();
        let frame = iframe_on(SessionHandle::new(conn.clone(), "S1"));
        let fut = tokio::spawn(async move { frame.evaluate_main::<i32>("1+1").await });

        serve_main_world_lookup(&mut mock, 3).await;
        let id = next_cmd(&mut mock, "Runtime.evaluate").await;
        assert_eq!(mock.last_sent()["params"]["expression"], "1+1");
        assert_eq!(mock.last_sent()["params"]["contextId"], 3);
        mock.reply(id, json!({ "result": { "value": 2, "type": "number" } }))
            .await;

        assert_eq!(fut.await.unwrap().unwrap(), 2);
        conn.shutdown();
    }

    /// When a navigation destroys the main world between the lookup and the
    /// evaluation, `Frame::evaluate_main` looks it up again, in an object
    /// group of its own, and evaluates there; it retries only once.
    #[tokio::test]
    async fn evaluate_main_on_an_iframe_retries_a_stale_context_once() {
        let (mut mock, conn) = MockConnection::pair();
        let frame = iframe_on(SessionHandle::new(conn.clone(), "S1"));
        let fut = tokio::spawn(async move { frame.evaluate_main::<i32>("1+1").await });

        let mut groups = Vec::new();
        for context in [3, 4] {
            groups.push(serve_main_world_lookup(&mut mock, context).await);
            let id = next_cmd(&mut mock, "Runtime.evaluate").await;
            assert_eq!(mock.last_sent()["params"]["contextId"], context);
            mock.reply_err(id, -32000, "Cannot find context with specified id")
                .await;
        }

        let err = fut.await.unwrap().unwrap_err();
        assert!(crate::isolated_world::is_stale_context(&err), "{err:?}");
        assert_ne!(groups[0], groups[1]);
        conn.shutdown();
    }

    /// `Frame::find().css(...).one()` dispatches `Runtime.evaluate` on the
    /// frame's session (same MockConnection in tests — so the assertion is
    /// "the call was made", not "it landed on a specific session id"). The
    /// resolved `Element` is wrapped via `synthesize_query`, which upgrades
    /// the frame's `Weak<TabInner>` — so the test frame is constructed with
    /// a live Tab backing rather than the `Weak::new()` shortcut used by
    /// `frame_on` in the evaluate-only tests above.
    ///
    /// We assert the full CDP trace mirrors the Tab/Element resolve_many
    /// path (Runtime.evaluate → Runtime.getProperties → DOM.describeNode)
    /// because Frame scope routes through the same `resolve_css_many` →
    /// `extract_array_refs` pipeline as Tab/Element scope, only swapping
    /// the underlying `SessionHandle`.
    #[tokio::test]
    async fn find_dispatches_runtime_evaluate_on_frames_session() {
        use crate::tab::Tab;

        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess.clone());
        // Construct the Frame with a live `Weak<TabInner>` so that
        // `synthesize_query` → `tab_for_synthesize` succeeds. We can't
        // use `Tab::main_frame()` here because that would consume the
        // mock connection's first inbound `Page.getFrameTree` slot.
        let frame = Frame::new(
            "FRAME_FIND".to_string(),
            None,
            String::new(),
            None,
            sess,
            std::sync::Arc::downgrade(&tab.inner),
        );

        let fut = tokio::spawn({
            let f = frame.clone();
            async move { f.find().css("button").one().await }
        });

        // Frame-scoped queries first allocate an isolated world for the
        // frame (so `Runtime.evaluate` runs against the frame's document
        // rather than the session's default — i.e. the parent tab's
        // main frame). Reply with a stub executionContextId so the
        // selector can attach it to the evaluate call below.
        let id_iso = next_cmd(&mut mock, "Page.createIsolatedWorld").await;
        assert_eq!(mock.last_sent()["params"]["frameId"], "FRAME_FIND");
        mock.reply(id_iso, json!({ "executionContextId": 4242 }))
            .await;

        // Next dispatch: Runtime.evaluate with the document.querySelectorAll
        // expression (resolve_css_many goes through the array path), now
        // pinned to the frame's isolated-world contextId.
        let id_q = next_cmd(&mut mock, "Runtime.evaluate").await;
        assert_eq!(mock.last_sent()["params"]["contextId"], 4242);
        let sent = mock.last_sent()["params"]["expression"]
            .as_str()
            .expect("expression should be a string")
            .to_string();
        assert!(
            sent.contains("document.querySelectorAll") && sent.contains("button"),
            "frame.find().css('button').one() must dispatch document.querySelectorAll, got: {sent}"
        );
        mock.reply(
            id_q,
            json!({ "result": { "objectId": "RArrF", "type": "object", "subtype": "array" } }),
        )
        .await;

        // Enumerate the array — one match at index 0.
        let id_p = next_cmd(&mut mock, "Runtime.getProperties").await;
        assert_eq!(mock.last_sent()["params"]["objectId"], "RArrF");
        mock.reply(
            id_p,
            json!({
                "result": [
                    {
                        "name": "0",
                        "value": { "objectId": "RFN0", "type": "object", "subtype": "node" }
                    },
                    {
                        "name": "length",
                        "value": { "value": 1, "type": "number" }
                    }
                ]
            }),
        )
        .await;

        // describeNode resolves the backendNodeId for the picked element.
        let id_d = next_cmd(&mut mock, "DOM.describeNode").await;
        assert_eq!(mock.last_sent()["params"]["objectId"], "RFN0");
        mock.reply(id_d, json!({ "node": { "backendNodeId": 77 } }))
            .await;

        let el = fut.await.expect("task should not panic").expect("one() ok");
        assert_eq!(
            el.inner.remote_object_id.lock().await.as_deref(),
            Some("RFN0")
        );
        assert_eq!(*el.inner.backend_node_id.lock().await, Some(77));
        conn.shutdown();
    }

    /// Main-frame `goto` is `Tab::goto` on the owning tab: `Page.enable`
    /// first, then `Page.navigate { url }` on the tab's session.
    #[tokio::test]
    async fn main_frame_goto_dispatches_page_navigate() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess.clone());
        let frame = Frame::new(
            tab.target_id().to_string(),
            None,
            String::new(),
            None,
            sess,
            Arc::downgrade(&tab.inner),
        );

        let fut = tokio::spawn({
            let f = frame.clone();
            async move { f.goto("https://example.com").await }
        });

        let id_enable = next_cmd(&mut mock, "Page.enable").await;
        mock.reply(id_enable, json!({})).await;

        let id_nav = next_cmd(&mut mock, "Page.navigate").await;
        assert_eq!(mock.last_sent()["params"]["url"], "https://example.com");
        mock.reply(id_nav, json!({ "frameId": tab.target_id() }))
            .await;

        fut.await.unwrap().unwrap();
        conn.shutdown();
    }

    /// A main frame whose tab is gone can't navigate: there is no tab to
    /// arm `wait_for_load` on.
    #[tokio::test]
    async fn main_frame_goto_errors_once_its_tab_is_gone() {
        let (_mock, conn) = MockConnection::pair();
        let frame = frame_on(SessionHandle::new(conn.clone(), "S1"), "FRAME_MAIN");
        let res = frame.goto("https://example.com").await;
        assert!(
            matches!(&res, Err(ZendriverError::Navigation(m)) if m.contains("tab is gone")),
            "unexpected result: {res:?}"
        );
        conn.shutdown();
    }

    /// Sub-frame `goto` is rejected up-front (no CDP traffic) with a
    /// `Navigation` error pointing callers at `evaluate_main` on the
    /// parent.
    #[tokio::test]
    async fn sub_frame_goto_returns_navigation_error() {
        let (_mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        // parent_frame_id = Some(...) → is_main() == false.
        let frame = Frame::new(
            "FRAME_CHILD".to_string(),
            Some("FRAME_PARENT".to_string()),
            String::new(),
            None,
            sess,
            Weak::new(),
        );

        let res = frame.goto("https://example.com").await;
        match res {
            Err(ZendriverError::Navigation(m)) => {
                assert!(
                    m.contains("sub-frame goto not supported"),
                    "unexpected message: {m}"
                );
            }
            other => panic!("expected Navigation error, got: {other:?}"),
        }
        conn.shutdown();
    }

    // --- a rejected frame id ----------------------------------------------

    /// Chrome keeps a frame's id for the frame's whole life, so when it
    /// rejects the id, the frame is gone from this session: the call fails
    /// rather than binding to another frame of the tree.
    #[tokio::test]
    async fn a_rejected_frame_id_is_not_found() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = Frame::new(
            "GONE".into(),
            Some("PARENT".into()),
            String::new(),
            None,
            sess,
            Weak::new(),
        );

        let fut = tokio::spawn({
            let f = frame.clone();
            async move { f.ensure_isolated_world().await }
        });
        let id_world = next_cmd(&mut mock, "Page.createIsolatedWorld").await;
        mock.reply_err(id_world, -32602, "No frame for given id found")
            .await;

        match fut.await.unwrap() {
            Err(ZendriverError::FrameNotFound(m)) => assert!(m.contains("GONE"), "{m}"),
            other => panic!("expected FrameNotFound, got: {other:?}"),
        }
        assert_eq!(mock.try_recv_cmd(), None, "no frame-tree walk");
        conn.shutdown();
    }
}
