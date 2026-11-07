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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use zendriver_transport::SessionHandle;

use crate::error::{Result, ZendriverError};
use crate::load_tracker::next_stop;
use crate::tab::{Tab, TabInner};

pub mod lifecycle;
pub mod oopif;
pub(crate) mod tree;

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
    /// The `frameId` that Chrome uses for this frame now, once Chrome has
    /// rejected `frame_id` (see [`Frame::id`]). Set by
    /// [`Frame::ensure_isolated_world`], so that its later calls skip the
    /// rejected id. `None` until then.
    pub(crate) rebound_frame_id: Mutex<Option<String>>,
    /// Set by [`lifecycle`] when Chrome reports this frame detached
    /// (`Page.frameDetached`). Then [`Frame::ensure_isolated_world`] fails
    /// with [`ZendriverError::FrameNotFound`] instead of rebinding this
    /// handle to another frame. Best effort: the subscriber can miss the
    /// event (see the [`lifecycle`] module docs).
    pub(crate) detached: AtomicBool,
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
                rebound_frame_id: Mutex::new(None),
                detached: AtomicBool::new(false),
                tab,
            }),
        }
    }

    /// The frame's CDP `frameId`, as recorded when this handle was built.
    ///
    /// The value never changes on a given handle, which is not the same as
    /// naming the frame forever. Chrome sometimes re-attaches an iframe
    /// under a fresh `frameId` with no `Page.frameDetached` in between (seen
    /// on `srcdoc` iframes under `--headless=new`), and this handle goes on
    /// reporting the id it was built with. [`Frame::evaluate`] and the query
    /// methods recover on their own by rebinding against the live frame
    /// tree; a caller that hands this id to CDP itself can instead get
    /// "No frame for given id found".
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
    /// Each call first gets the id of the isolated world's context in the
    /// frame's current document (see [`Frame::ensure_isolated_world`]), so a
    /// `Frame` handle kept across a navigation of its frame keeps working.
    /// If a navigation destroys that context before the expression reaches
    /// it, Chrome answers `Cannot find context with specified id`, and the
    /// call is retried once.
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
        for attempt in 0..2 {
            let ctx_id = self.ensure_isolated_world().await?;
            let res = self
                .inner
                .session
                .call(
                    "Runtime.evaluate",
                    json!({
                        "expression": js,
                        "contextId": ctx_id,
                        "returnByValue": true,
                        "awaitPromise": true,
                    }),
                )
                .await;
            match res {
                Ok(v) => return Self::extract_value(&v),
                Err(e) => {
                    // The raw session answers with a `CallError`.
                    let e = ZendriverError::from(e);
                    if attempt > 0 || !crate::isolated_world::is_stale_context(&e) {
                        return Err(e);
                    }
                    // A navigation destroyed the context after
                    // `ensure_isolated_world` returned its id; the next
                    // iteration gets the new document's.
                }
            }
        }
        unreachable!("the retry loop returns on both the Ok and the second-attempt Err arm")
    }

    /// Evaluate JS in this frame's **main world** (page globals visible).
    ///
    /// Escape hatch when isolated-world semantics don't fit. Dispatches
    /// `Runtime.evaluate` on the frame's session: with no `contextId` for
    /// the top frame of that session (the tab's main frame, or an OOPIF,
    /// whose session is a distinct child session), and with the id of the
    /// frame's main world for any other iframe. `js` is a script, as for
    /// [`crate::Tab::evaluate_main`]: the value of its last statement is
    /// the result.
    ///
    /// # Errors
    ///
    /// Returns [`ZendriverError::JsException`] when the expression raises.
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
        let mut params = json!({
            "expression": js.as_ref(),
            "returnByValue": true,
            "awaitPromise": true,
        });
        if let Some(context_id) = self.main_world_context_id().await? {
            params["contextId"] = json!(context_id);
        }
        let res = self.inner.session.call("Runtime.evaluate", params).await?;
        Self::extract_value(&res)
    }

    /// The `executionContextId` of this frame's main world, or `None` when
    /// the frame is the top frame of its session, where `Runtime.evaluate`
    /// with no `contextId` already runs in the frame's main world.
    ///
    /// Found without `Runtime.enable` (which pages can detect): the frame's
    /// owner element gives its document, `DOM.resolveNode` with no context
    /// id wraps that document in the main world of the document's own
    /// frame, and the id of the returned object holds the context's id (see
    /// [`context_id_of_object`]).
    ///
    /// Asks Chrome on every call, for the reason given on
    /// [`Frame::ensure_isolated_world`].
    ///
    /// # Errors
    ///
    /// The error of any of the calls, for example when the frame is gone;
    /// or [`ZendriverError::Navigation`] when a reply lacks what it should
    /// hold, for example when the frame's document is in another process.
    async fn main_world_context_id(&self) -> Result<Option<i64>> {
        if self.inner.parent_frame_id.is_none() {
            return Ok(None);
        }
        let session = &self.inner.session;
        let rebound = self.inner.rebound_frame_id.lock().await.clone();
        let frame_id = rebound.as_deref().unwrap_or(&self.inner.frame_id);
        let missing = |what: &str| ZendriverError::Navigation(format!("frame {frame_id}: {what}"));

        let tree = session.call("Page.getFrameTree", json!({})).await?;
        if tree["frameTree"]["frame"]["id"].as_str() == Some(frame_id) {
            return Ok(None);
        }
        let owner = session
            .call("DOM.getFrameOwner", json!({ "frameId": frame_id }))
            .await?;
        let owner = owner["backendNodeId"]
            .as_i64()
            .ok_or_else(|| missing("DOM.getFrameOwner returned no backendNodeId"))?;
        let described = session
            .call("DOM.describeNode", json!({ "backendNodeId": owner }))
            .await?;
        let document = described["node"]["contentDocument"]["backendNodeId"]
            .as_i64()
            .ok_or_else(|| missing("its owner element has no document in this process"))?;
        let resolved = session
            .call("DOM.resolveNode", json!({ "backendNodeId": document }))
            .await?;
        let object_id = resolved["object"]["objectId"]
            .as_str()
            .ok_or_else(|| missing("DOM.resolveNode returned no objectId for its document"))?;
        let context_id = context_id_of_object(object_id);
        session
            .call("Runtime.releaseObject", json!({ "objectId": object_id }))
            .await?;
        context_id.map(Some)
    }

    /// The frame's current document HTML.
    ///
    /// Returns the serialized `document.documentElement.outerHTML`. Backed
    /// by [`Frame::evaluate_main`], so for any iframe, in the page's process
    /// or not, this is the iframe's *own* HTML, not the parent page's.
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
    /// Asks Chrome on every call (one `Page.createIsolatedWorld`), because a
    /// remembered id can name a different context: a cross-process
    /// navigation numbers the new process's contexts from 1 again, so an id
    /// from the previous document can belong to, say, an iframe of the new
    /// one, and a call with it then runs there without any error. Chrome
    /// keeps one isolated world per name, so a repeated call returns the
    /// same context and creates nothing. The same can still happen to the
    /// returned id if such a navigation commits before the caller uses it,
    /// but only within that one round trip.
    ///
    /// If Chrome has re-attached the frame under a fresh `frameId` (observed
    /// for `srcdoc` iframes under `--headless=new`, with no `frameDetached`
    /// in between), it rejects the recorded id with "No frame for given id
    /// found". Then this finds the live id in the frame tree, remembers it
    /// for later calls, and retries with it. Unless Chrome reported the
    /// frame detached: then the frame is gone, and a frame found in the
    /// tree would be another one.
    pub(crate) async fn ensure_isolated_world(&self) -> Result<i64> {
        let rebound = self.inner.rebound_frame_id.lock().await.clone();
        let frame_id = rebound.as_deref().unwrap_or(&self.inner.frame_id);
        match self.create_isolated_world(frame_id).await {
            Err(ZendriverError::Cdp {
                code: -32602,
                ref message,
                ..
            }) if message.contains("No frame for given id") => {}
            other => return other,
        }
        if self.inner.detached.load(Ordering::Relaxed) {
            return Err(ZendriverError::FrameNotFound(format!(
                "frame {} was detached",
                self.inner.frame_id
            )));
        }
        let live = self.discover_current_frame_id().await?;
        let ctx = self.create_isolated_world(&live).await?;
        *self.inner.rebound_frame_id.lock().await = Some(live);
        Ok(ctx)
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

    /// Find the frame id Chrome currently uses for this frame, by locating
    /// it in the live `Page.getFrameTree`.
    ///
    /// Only [`FrameInner::rebound_frame_id`] records the result.
    /// [`FrameInner::frame_id`] is immutable, so [`Frame::id`] keeps
    /// returning the id Chrome rejected, and so does anything filtering on
    /// it — notably [`Frame::wait_for_load`], whose
    /// `Page.frameStoppedLoading` filter can then never match and always
    /// burns its full timeout. `evaluate` / `find` recover because they
    /// go through [`Frame::ensure_isolated_world`]; nothing that reads the
    /// id does. Making `frame_id` interior-mutable would fix that too, and
    /// is a separate change.
    ///
    /// The probe runs on the transport's default call budget
    /// ([`zendriver_transport::DEFAULT_CALL_TIMEOUT`]), like every other CDP
    /// call the caller is awaiting. [`crate::frame::lifecycle`] bounds its
    /// copy of the same `Page.getFrameTree` far tighter, and the difference
    /// is deliberate: that one runs in a detached task with nobody waiting
    /// on it, so its budget exists to reclaim the task rather than to answer
    /// a caller.
    ///
    /// A single child under the recorded parent is unambiguous. With
    /// several, the recorded `name` and `url` are the only things that tell
    /// them apart — and when neither singles one out, this returns
    /// [`ZendriverError::FrameNotFound`] instead of binding the first
    /// depth-first hit. A wrong bind is worse than an error: it reports
    /// `Ok` and then runs every later `evaluate` / query against a
    /// different iframe's document.
    async fn discover_current_frame_id(&self) -> Result<String> {
        /// One candidate child frame, as reported by `Page.getFrameTree`.
        struct Child {
            /// The live CDP `frameId` — what a successful recovery binds.
            id: String,
            /// Chrome's reported frame name, absent when it sends none.
            /// Compared against our own recorded name below.
            name: Option<String>,
            /// Chrome's reported document URL, absent when it sends none.
            /// Empty for a frame that has not navigated.
            url: Option<String>,
        }
        /// The one candidate satisfying `pred`, or `None` when zero or
        /// several do — "several" must not resolve to "the first".
        fn sole_match(candidates: &[Child], pred: impl Fn(&Child) -> bool) -> Option<&Child> {
            let mut hits = candidates.iter().filter(|c| pred(c));
            let first = hits.next()?;
            hits.next().is_none().then_some(first)
        }

        let parent = self.inner.parent_frame_id.as_deref().ok_or_else(|| {
            ZendriverError::Navigation(
                "frame_id rejected and no parent_frame_id recorded to recover from".into(),
            )
        })?;
        let tree = self
            .inner
            .session
            .call("Page.getFrameTree", json!({}))
            .await?;
        // EVERY child of `parent`, not just the first — the count is what
        // reveals ambiguity. The root is excluded because it is the tree's
        // own frame rather than a child of anything in the tree.
        //
        // That exclusion also costs the OOPIF case, which is why it is
        // written as a depth check and not folded into the `parent_id`
        // comparison. Walked on an OOPIF's own child session, the tree's
        // root IS this frame and carries `parent` as its `parentId` — so
        // dropping the depth check is exactly what an OOPIF would need, and
        // it is not done here because the shape of that tree is unverified
        // against a real browser. Guessing wrong binds a live-looking id
        // from the wrong target, and the paragraph above says why a wrong
        // bind is worse than the `FrameNotFound` an OOPIF gets today.
        let mut candidates = Vec::new();
        tree::walk(&tree["frameTree"], &mut |node| {
            if node.depth > 0 && node.parent_id == Some(parent) {
                candidates.push(Child {
                    id: node.id.to_string(),
                    name: node.name.map(str::to_string),
                    url: node.url.map(str::to_string),
                });
            }
        });

        if candidates.is_empty() {
            return Err(ZendriverError::FrameNotFound(format!(
                "no child frame found under parent {parent} during recovery"
            )));
        }
        if candidates.len() == 1 {
            return Ok(candidates.swap_remove(0).id);
        }
        // Several siblings: only a unique name or url match may bind.
        let name = self.inner.name.as_deref().filter(|n| !n.is_empty());
        if let Some(name) = name {
            if let Some(hit) = sole_match(&candidates, |c| c.name.as_deref() == Some(name)) {
                return Ok(hit.id.clone());
            }
        }
        let url = self.inner.url.read().await.clone();
        if !url.is_empty() {
            if let Some(hit) = sole_match(&candidates, |c| c.url.as_deref() == Some(url.as_str())) {
                return Ok(hit.id.clone());
            }
        }
        Err(ZendriverError::FrameNotFound(format!(
            "frame id recovery under parent {parent} is ambiguous: {} sibling frames, and neither \
             the recorded name ({}) nor url ({}) matches exactly one",
            candidates.len(),
            name.unwrap_or("<none>"),
            if url.is_empty() { "<none>" } else { &url },
        )))
    }

    /// Shared post-processing for `Runtime.evaluate` responses — checks
    /// `exceptionDetails` first (raising [`ZendriverError::JsException`])
    /// then deserializes `result.value` into `T`. Mirrors the inline tail
    /// of [`crate::tab::Tab::evaluate`] / `evaluate_main`.
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

/// The id of the execution context that the V8 remote object `object_id`
/// lives in.
///
/// V8 writes an object id as `<isolate id>.<context id>.<object number>`
/// (its `RemoteObjectId`). The protocol doesn't document that format, so
/// this fails loudly rather than guessing when an id doesn't have it.
///
/// # Errors
///
/// [`ZendriverError::Navigation`] when `object_id` isn't three `.`-separated
/// parts with an integer in the middle.
fn context_id_of_object(object_id: &str) -> Result<i64> {
    let parts: Vec<&str> = object_id.split('.').collect();
    let context = match parts.as_slice() {
        [_isolate, context, _object] => context.parse().ok(),
        _ => None,
    };
    context.ok_or_else(|| {
        ZendriverError::Navigation(format!(
            "can't read an execution context id out of the remote object id {object_id:?}"
        ))
    })
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
    async fn evaluate_recreates_the_isolated_world_after_a_stale_context() {
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

        // The retry: a NEW isolated world, then the same expression again.
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

    #[test]
    fn context_id_of_object_reads_the_middle_of_three_parts() {
        assert_eq!(context_id_of_object("5137660561264051122.3.1").unwrap(), 3);
        for bad in [
            "3",
            "1.3",
            "1.x.2",
            "1.3.2.4",
            r#"{"injectedScriptId":3,"id":1}"#,
        ] {
            assert!(
                matches!(
                    context_id_of_object(bad),
                    Err(ZendriverError::Navigation(_))
                ),
                "{bad}"
            );
        }
    }

    /// `Frame::evaluate_main` dispatches `Runtime.evaluate` directly on the
    /// frame's session with NO `contextId` parameter — proving the
    /// main-world path skips the isolated-world handshake entirely (no
    /// `Page.createIsolatedWorld` in the trace).
    #[tokio::test]
    async fn evaluate_main_dispatches_runtime_evaluate_without_context_id() {
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

    // --- stale frame-id recovery (`discover_current_frame_id`) ----------

    /// A `Frame` whose recorded id Chrome will reject, forcing
    /// `ensure_isolated_world` down the recovery path.
    fn stale_frame(session: SessionHandle, name: Option<&str>, url: &str) -> Frame {
        Frame::new(
            "STALE".to_string(),
            Some("PARENT".to_string()),
            url.to_string(),
            name.map(str::to_string),
            session,
            Weak::new(),
        )
    }

    /// Drain the rejected first `Page.createIsolatedWorld` and answer the
    /// recovery `Page.getFrameTree` with `children` under `PARENT`.
    async fn reject_then_serve_tree(mock: &mut MockConnection, children: Value) {
        let id_world = next_cmd(mock, "Page.createIsolatedWorld").await;
        assert_eq!(mock.last_sent()["params"]["frameId"], "STALE");
        mock.reply_err(id_world, -32602, "No frame for given id found")
            .await;
        let id_tree = next_cmd(mock, "Page.getFrameTree").await;
        mock.reply(
            id_tree,
            json!({
                "frameTree": {
                    "frame": { "id": "PARENT", "url": "https://host.test/" },
                    "childFrames": children,
                }
            }),
        )
        .await;
    }

    fn tree_frame(id: &str, name: &str, url: &str) -> Value {
        json!({ "frame": { "id": id, "parentId": "PARENT", "name": name, "url": url } })
    }

    /// Two sibling iframes and nothing to tell them apart: recovery must
    /// FAIL. Binding the first depth-first hit would return `Ok` and then
    /// run every later `evaluate` against the wrong document.
    #[tokio::test]
    async fn ambiguous_recovery_errors_instead_of_binding_a_sibling() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = stale_frame(sess, None, "");

        let fut = tokio::spawn({
            let f = frame.clone();
            async move { f.ensure_isolated_world().await }
        });
        reject_then_serve_tree(
            &mut mock,
            json!([
                tree_frame("F1", "", "https://host.test/a"),
                tree_frame("F2", "", "https://host.test/b"),
            ]),
        )
        .await;

        match fut.await.unwrap() {
            Err(ZendriverError::FrameNotFound(m)) => {
                assert!(m.contains("ambiguous"), "unexpected message: {m}");
            }
            other => panic!("expected FrameNotFound on an ambiguous tree, got: {other:?}"),
        }
        assert_eq!(
            mock.try_recv_cmd(),
            None,
            "a failed recovery must not retry Page.createIsolatedWorld against a guess",
        );
        conn.shutdown();
    }

    /// The recorded frame `name` singles one sibling out — that is a bind
    /// we can justify, so recovery proceeds with the live id.
    #[tokio::test]
    async fn recovery_uses_the_recorded_name_to_pick_among_siblings() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = stale_frame(sess, Some("sidebar"), "");

        let fut = tokio::spawn({
            let f = frame.clone();
            async move { f.ensure_isolated_world().await }
        });
        reject_then_serve_tree(
            &mut mock,
            json!([
                tree_frame("F1", "banner", "https://host.test/a"),
                tree_frame("F2", "sidebar", "https://host.test/b"),
            ]),
        )
        .await;

        let id_retry = next_cmd(&mut mock, "Page.createIsolatedWorld").await;
        assert_eq!(
            mock.last_sent()["params"]["frameId"],
            "F2",
            "recovery must bind the name-matched sibling",
        );
        mock.reply(id_retry, json!({ "executionContextId": 99 }))
            .await;
        assert_eq!(fut.await.unwrap().unwrap(), 99);
        conn.shutdown();
    }

    /// After a recovery, later calls go straight to the live id, instead of
    /// being rejected on the recorded one and walking the frame tree again.
    #[tokio::test]
    async fn recovery_remembers_the_live_id() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = stale_frame(sess, None, "");

        let fut = tokio::spawn({
            let f = frame.clone();
            async move { f.ensure_isolated_world().await }
        });
        reject_then_serve_tree(&mut mock, json!([tree_frame("LIVE", "", "")])).await;
        let id_retry = next_cmd(&mut mock, "Page.createIsolatedWorld").await;
        mock.reply(id_retry, json!({ "executionContextId": 3 }))
            .await;
        assert_eq!(fut.await.unwrap().unwrap(), 3);

        let fut = tokio::spawn({
            let f = frame.clone();
            async move { f.ensure_isolated_world().await }
        });
        let id_world = next_cmd(&mut mock, "Page.createIsolatedWorld").await;
        assert_eq!(mock.last_sent()["params"]["frameId"], "LIVE");
        mock.reply(id_world, json!({ "executionContextId": 4 }))
            .await;
        assert_eq!(fut.await.unwrap().unwrap(), 4);
        conn.shutdown();
    }

    /// A frame that Chrome reported detached is gone: recovery must fail
    /// instead of walking the tree, where the lone child it would bind is
    /// another frame.
    #[tokio::test]
    async fn a_detached_frame_is_not_rebound() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = stale_frame(sess, None, "");
        frame.inner.detached.store(true, Ordering::Relaxed);

        let fut = tokio::spawn({
            let f = frame.clone();
            async move { f.ensure_isolated_world().await }
        });
        let id_world = next_cmd(&mut mock, "Page.createIsolatedWorld").await;
        mock.reply_err(id_world, -32602, "No frame for given id found")
            .await;

        match fut.await.unwrap() {
            Err(ZendriverError::FrameNotFound(m)) => assert!(m.contains("detached"), "{m}"),
            other => panic!("expected FrameNotFound, got: {other:?}"),
        }
        assert_eq!(mock.try_recv_cmd(), None, "no frame-tree walk");
        conn.shutdown();
    }

    /// No name, but the recorded url matches exactly one sibling.
    #[tokio::test]
    async fn recovery_falls_back_to_the_recorded_url() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = stale_frame(sess, None, "https://host.test/b");

        let fut = tokio::spawn({
            let f = frame.clone();
            async move { f.ensure_isolated_world().await }
        });
        reject_then_serve_tree(
            &mut mock,
            json!([
                tree_frame("F1", "", "https://host.test/a"),
                tree_frame("F2", "", "https://host.test/b"),
            ]),
        )
        .await;

        let id_retry = next_cmd(&mut mock, "Page.createIsolatedWorld").await;
        assert_eq!(mock.last_sent()["params"]["frameId"], "F2");
        mock.reply(id_retry, json!({ "executionContextId": 5 }))
            .await;
        assert_eq!(fut.await.unwrap().unwrap(), 5);
        conn.shutdown();
    }

    /// The unambiguous single-iframe page keeps recovering with no name or
    /// url to go on — the disambiguation must not make the common case
    /// stricter.
    #[tokio::test]
    async fn lone_child_still_recovers_without_name_or_url() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = stale_frame(sess, None, "");

        let fut = tokio::spawn({
            let f = frame.clone();
            async move { f.ensure_isolated_world().await }
        });
        reject_then_serve_tree(&mut mock, json!([tree_frame("LIVE", "", "")])).await;

        let id_retry = next_cmd(&mut mock, "Page.createIsolatedWorld").await;
        assert_eq!(mock.last_sent()["params"]["frameId"], "LIVE");
        mock.reply(id_retry, json!({ "executionContextId": 3 }))
            .await;
        assert_eq!(fut.await.unwrap().unwrap(), 3);
        conn.shutdown();
    }
}
