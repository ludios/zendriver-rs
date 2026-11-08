//! Selector kinds + CDP/JS resolution. Covers CSS, XPath, Text,
//! TextRegex, and Role selectors.
//!
//! Every entry point resolves through `SelectorKind::resolve_many_inner`:
//! `FindBuilder`'s `.one()` takes the best-match head, `FindAllBuilder`'s
//! `.many()` returns the whole vec, and `Element::refresh` drives
//! `resolve_many`. There is deliberately **no** single-match resolver
//! family. One existed until it was deleted: a parallel `resolve_one` /
//! `resolve_*_one` set that production never called, kept alive by
//! `#[allow(dead_code)]` and pointed at by six tests. It silently drifted
//! from the shipping path — a needle-normalization fix landed in both
//! copies, and only the dead one was covered, so deleting the fix from the
//! live path left the whole suite green. Resolver tests must therefore
//! drive `resolve_many` / `resolve_many_inner`; if a single-match
//! specialization is ever wanted for its cheaper wire shape, it belongs
//! behind `resolve_many_inner` rather than beside it.
//!
//! Role resolution asks Chrome's accessibility tree
//! (`Accessibility.queryAXTree`), which knows the roles that elements have
//! implicitly (`<a href>` is a link) and their computed accessible names;
//! see `resolve_role_many`.

use std::time::Duration;

use serde_json::{Value, json};
use tracing::warn;
use zendriver_transport::{CallError, SessionHandle};

use crate::element::Element;
use crate::error::{Result, ZendriverError};
use crate::frame::Frame;
use crate::query::role::AriaRole;
use crate::tab::Tab;

/// A resolved CDP node handle. CSS / XPath / text / role queries all
/// hand back one (or many) of these; the caller wraps them into
/// `Element` values.
#[derive(Debug, Clone)]
pub(crate) struct RemoteRef {
    pub(crate) remote_object_id: String,
    pub(crate) backend_node_id: i64,
}

/// What the query runs against: a whole tab (root = `document`), a
/// subtree rooted at an existing element (root = `this`), or a specific
/// frame (root = the frame's `document`, dispatched on the frame's own
/// CDP session — distinct from the parent tab's session for OOPIFs).
pub(crate) enum QueryScope<'a> {
    Tab(&'a Tab),
    Element(&'a Element),
    Frame(&'a Frame),
}

impl QueryScope<'_> {
    /// CDP session that should dispatch this query's commands. Tab and
    /// Element scopes both route to the owning tab's session; Frame
    /// scope routes to the frame's own session (same as the parent tab
    /// for same-origin frames, a distinct child session for OOPIFs).
    ///
    /// All `Runtime.evaluate` / `Runtime.callFunctionOn` /
    /// `Runtime.getProperties` / `DOM.describeNode` /
    /// `Accessibility.queryAXTree` calls in this module go through
    /// this accessor so adding a new scope variant only requires
    /// extending the match arm.
    pub(crate) fn session(&self) -> &SessionHandle {
        match self {
            QueryScope::Tab(t) => t.session(),
            QueryScope::Element(e) => e.tab().session(),
            QueryScope::Frame(f) => f.session(),
        }
    }

    /// Execution-context id that a document-wide query's `Runtime.evaluate`
    /// targets: zendriver's isolated world in the current document of the
    /// tab's main frame (Tab scope) or of the frame (Frame scope).
    ///
    /// The isolated world has its own globals and prototypes, so a page that
    /// wraps `querySelectorAll`, `getBoundingClientRect` or the like neither
    /// sees the query nor can falsify it. The element handles that the
    /// query returns live in that world too, so everything later run on
    /// them (actionability checks, `focus`, the value setter, ...) stays
    /// there. A Frame scope also could not use the session's default
    /// context: for a same-origin iframe, that is the parent's document.
    ///
    /// `None` for an Element scope, whose queries call functions on the
    /// element's own handle instead, which is already in that world.
    pub(crate) async fn execution_context_id(&self) -> Result<Option<i64>> {
        match self {
            QueryScope::Tab(t) => Ok(Some(t.ensure_isolated_world().await?)),
            QueryScope::Frame(f) => Ok(Some(f.ensure_isolated_world().await?)),
            QueryScope::Element(_) => Ok(None),
        }
    }

    /// Owned `Tab` clone for `Element::synthesize_query`. Tab/Element
    /// scopes return a cheap `Arc` bump on the underlying `TabInner`;
    /// Frame scope upgrades the frame's `Weak<TabInner>` (which is
    /// always live in practice because every `Frame` is constructed by
    /// a `Tab` that holds the strong reference). A dead Weak indicates
    /// the owning Tab was dropped while the Frame clone outlived it —
    /// a logic bug worth a clear panic rather than a confusing
    /// `Result` propagation.
    pub(crate) fn synthesize_tab(&self) -> Tab {
        match self {
            QueryScope::Tab(t) => (*t).clone(),
            QueryScope::Element(e) => e.tab().clone(),
            QueryScope::Frame(f) => f
                .tab_for_synthesize()
                .expect("Frame outlived its owning Tab while a FindBuilder query was in flight"),
        }
    }
}

/// The kind of selector that a `FindBuilder` / `FindAllBuilder` query
/// resolves.
///
/// `TextRegex` holds the JavaScript source and flags that the page passes
/// to `new RegExp(pattern, flags)`.
#[derive(Debug, Clone)]
pub(crate) enum SelectorKind {
    Css(String),
    Xpath(String),
    Text { needle: String, exact: bool },
    TextRegex { pattern: String, flags: String },
    Role(AriaRole, Option<String>),
}

impl SelectorKind {
    /// Resolve this selector against `scope` and return every match in
    /// document order. Empty `Vec` for no matches (not an error).
    ///
    /// This is the only resolution entry point. `.one()` is
    /// [`Self::resolve_many_inner`] plus a take-first, so there is no
    /// separate single-match implementation that could drift from it —
    /// see the module header.
    pub(crate) async fn resolve_many(&self, scope: &QueryScope<'_>) -> Result<Vec<RemoteRef>> {
        self.resolve_many_inner(scope, false).await
    }

    /// `resolve_many` with the cross-cutting `best_match` flag. It only
    /// affects text selectors (`Text` / `TextRegex`), where the JS
    /// collector re-sorts candidates by closest text length; it is a
    /// no-op for css/xpath/role. When set, the returned Vec is ordered
    /// closest-length first, so `.one()` taking `[0]` lands on the
    /// nearest match.
    pub(crate) async fn resolve_many_inner(
        &self,
        scope: &QueryScope<'_>,
        best_match: bool,
    ) -> Result<Vec<RemoteRef>> {
        match self {
            SelectorKind::Css(sel) => resolve_css_many(scope, sel).await,
            SelectorKind::Xpath(expr) => resolve_xpath_many(scope, expr).await,
            SelectorKind::Text { needle, exact } => {
                resolve_text_many(scope, needle, *exact, best_match).await
            }
            SelectorKind::TextRegex { pattern, flags } => {
                resolve_text_regex_many(scope, pattern, flags, best_match).await
            }
            SelectorKind::Role(role, name) => {
                resolve_role_many(scope, *role, name.as_deref()).await
            }
        }
    }
}

// ---------------------------------------------------------------------
// Shared eval-in-scope helper
// ---------------------------------------------------------------------

/// Evaluate `expression` via `Runtime.evaluate` in `scope`'s execution
/// context (see [`QueryScope::execution_context_id`]), so that a bare
/// `document` in it is the scope's document, seen from zendriver's
/// isolated world.
///
/// If a navigation destroys that context between looking it up and the
/// evaluation, the call is retried once in the new document's context.
/// Without the retry, a `.one()` polling across a navigation could fail
/// instead of waiting for the element on the next page.
///
/// Every `Tab`/`Frame` match arm below (css/xpath/text/text_regex/
/// predicate, and role for its root `document`) routes through this one
/// function instead of hand-rolling the
/// `contextId` dance per resolver.
async fn eval_expr_in_scope(scope: &QueryScope<'_>, expression: String) -> Result<Value> {
    let expression = expression.as_str();
    crate::isolated_world::retry_once_on_stale_context(|| async move {
        let mut params = json!({
            "expression": expression,
            "returnByValue": false,
        });
        if let Some(id) = scope.execution_context_id().await? {
            params["contextId"] = json!(id);
        }
        Ok(scope.session().call("Runtime.evaluate", params).await?)
    })
    .await
}

// ---------------------------------------------------------------------
// CSS
// ---------------------------------------------------------------------

async fn resolve_css_many(scope: &QueryScope<'_>, selector: &str) -> Result<Vec<RemoteRef>> {
    let session = scope.session();
    let result = match scope {
        QueryScope::Tab(_) | QueryScope::Frame(_) => {
            eval_expr_in_scope(
                scope,
                format!("Array.from(document.querySelectorAll({}))", json!(selector)),
            )
            .await?
        }
        QueryScope::Element(el) => {
            let object_id = el.remote_object_id_cloned().await?;
            session
                .call(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object_id,
                        "functionDeclaration": "function(s){return Array.from(this.querySelectorAll(s));}",
                        "arguments": [{ "value": selector }],
                        "returnByValue": false,
                    }),
                )
                .await?
        }
    };
    extract_array_refs(session, &result).await
}

// ---------------------------------------------------------------------
// Predicate (bs4-like combinable matchers)
// ---------------------------------------------------------------------
//
// A `PredicateSet` compiles to a CSS selector (`tag` + structural attrs)
// plus a JS post-filter function (`attr_regex` + text predicates). The
// terminal builds ONE `querySelectorAll(css).filter(<jsFilter>)` per
// scope — exactly mirroring `resolve_css_many`'s scope dispatch
// (`contextId` for frames, `this` for element subtrees) so predicates
// cross frames the same way CSS does.

use crate::query::predicate::PredicateSet;

/// Resolve `pred` against `scope` and return every match in document
/// order. Compiles to `Array.from((document|this).querySelectorAll(css))
/// .filter(<jsFilter>)` (without the `.filter` when `pred` has no
/// post-filter), with the CSS selector JSON-embedded so a quote/backslash
/// in an attribute value can't break out of the JS string literal. Empty
/// `Vec` for no matches (not an error).
pub(crate) async fn resolve_predicate_many(
    scope: &QueryScope<'_>,
    pred: &PredicateSet,
) -> Result<Vec<RemoteRef>> {
    let session = scope.session();
    let css = pred.to_css_selector();
    let mut filter = String::new();
    if let Some(f) = pred.to_js_filter() {
        filter = format!(".filter({f})");
    }
    let result = match scope {
        QueryScope::Tab(_) | QueryScope::Frame(_) => {
            let expr = format!(
                "Array.from(document.querySelectorAll({})){filter}",
                json!(css)
            );
            eval_expr_in_scope(scope, expr).await?
        }
        QueryScope::Element(el) => {
            let object_id = el.remote_object_id_cloned().await?;
            let func = format!(
                "function(){{return Array.from(this.querySelectorAll({})){filter};}}",
                json!(css)
            );
            session
                .call(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object_id,
                        "functionDeclaration": func,
                        "returnByValue": false,
                    }),
                )
                .await?
        }
    };
    extract_array_refs(session, &result).await
}

// ---------------------------------------------------------------------
// XPath
// ---------------------------------------------------------------------

async fn resolve_xpath_many(scope: &QueryScope<'_>, expr: &str) -> Result<Vec<RemoteRef>> {
    // Build an Array of nodes from an ORDERED_NODE_SNAPSHOT_TYPE result so
    // `extract_array_refs` can enumerate it via `Runtime.getProperties`.
    let session = scope.session();
    let result = match scope {
        QueryScope::Tab(_) | QueryScope::Frame(_) => {
            eval_expr_in_scope(
                scope,
                format!(
                    "(function(){{var r=document.evaluate({}, document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);var a=[];for(var i=0;i<r.snapshotLength;i++)a.push(r.snapshotItem(i));return a;}})()",
                    json!(expr)
                ),
            )
            .await?
        }
        QueryScope::Element(el) => {
            let object_id = el.remote_object_id_cloned().await?;
            session
                .call(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object_id,
                        "functionDeclaration":
                            "function(e){var r=document.evaluate(e, this, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);var a=[];for(var i=0;i<r.snapshotLength;i++)a.push(r.snapshotItem(i));return a;}",
                        "arguments": [{ "value": expr }],
                        "returnByValue": false,
                    }),
                )
                .await?
        }
    };
    extract_array_refs(session, &result).await
}

// ---------------------------------------------------------------------
// Text (case-insensitive substring or whitespace-collapsed exact)
// ---------------------------------------------------------------------
//
// Two paths:
//   - exact=true  -> an XPath built in Rust by `text_exact_xpath` and
//     evaluated with `ORDERED_NODE_SNAPSHOT_TYPE`. The needle is rendered as
//     an XPath literal by `xpath_string_literal` (which has the escaping
//     rule and the `concat()` fallback — do not restate it here). Tab and
//     frame scope emit `//*` against `document`; element scope emits the
//     relative `.//*` against `this`, or the walk leaves the element's
//     subtree.
//   - exact=false -> JS tree walk:
//     `Array.from(ctx.querySelectorAll('*')).filter(el => (el.innerText||el.textContent).toLowerCase().includes(needle.toLowerCase()))`.
//     Both paths return an array; `.one()` takes its first element.
//
// `innerText||textContent` matches Playwright's `getByText` and is
// resilient to hidden elements (which have `innerText === ""` but
// non-empty `textContent`).

/// JS fragment that re-sorts an array of elements ascending by
/// `abs(len(elementText) - needleLen)` — the nodriver "closest-length"
/// heuristic. Appended to the collector output only when `best_match`
/// is set so `.one()` (which takes `[0]`) lands on the nearest match.
/// `arr` is the in-scope identifier holding the candidate array.
fn best_match_sort_js(arr: &str, needle_len: usize) -> String {
    format!(
        "{arr}.sort(function(a,b){{\
            var la=(a.innerText||a.textContent||'').length;\
            var lb=(b.innerText||b.textContent||'').length;\
            return Math.abs(la-{n})-Math.abs(lb-{n});\
        }})",
        arr = arr,
        n = needle_len,
    )
}

fn build_text_substring_js_tab(needle: &str, best_match: bool) -> String {
    // Narrowest-match filter: every element whose own text contains the
    // needle, MINUS any element that also has a descendant whose text
    // contains the needle. Without the narrowing step, the naive filter
    // returns `[html, body, ...ancestors..., target]` in document
    // order — `.one()` then picks `<html>` and the caller's
    // `el.attr("id")` returns None even though the test's `<button id=…>`
    // matched the needle.
    //
    // When `best_match` is set, the narrowed array is re-sorted ascending
    // by `abs(len(text) - len(needle))` so `.one()` (which takes `[0]`)
    // returns the closest-length candidate rather than the first in
    // document order.
    let sort = if best_match {
        format!(";{}", best_match_sort_js("r", needle.chars().count()))
    } else {
        String::new()
    };
    format!(
        "(function(){{\
            var n={n};\
            var lc=n.toLowerCase();\
            var matches=Array.from(document.querySelectorAll('*')).filter(function(el){{\
                var t=el.innerText||el.textContent||'';\
                return t.toLowerCase().includes(lc);\
            }});\
            var r=matches.filter(function(el){{\
                return !Array.from(el.querySelectorAll('*')).some(function(c){{\
                    var t=c.innerText||c.textContent||'';\
                    return t.toLowerCase().includes(lc);\
                }});\
            }}){sort};\
            return r;\
        }})()",
        n = json!(needle),
        sort = sort,
    )
}

fn build_text_substring_fn_body(needle: &str, best_match: bool) -> String {
    // Element scope: `this` is the scope element. Used via
    // `Runtime.callFunctionOn` with the needle as the sole argument.
    // Same narrowing (+ optional best_match sort) semantics as the
    // tab/frame path above.
    let sort = if best_match {
        format!(";{}", best_match_sort_js("r", needle.chars().count()))
    } else {
        String::new()
    };
    format!(
        "function(n){{\
            var lc=n.toLowerCase();\
            var matches=Array.from(this.querySelectorAll('*')).filter(function(el){{\
                var t=el.innerText||el.textContent||'';\
                return t.toLowerCase().includes(lc);\
            }});\
            var r=matches.filter(function(el){{\
                return !Array.from(el.querySelectorAll('*')).some(function(c){{\
                    var t=c.innerText||c.textContent||'';\
                    return t.toLowerCase().includes(lc);\
                }});\
            }}){sort};\
            return r;\
        }}",
        sort = sort,
    )
}

/// Render `s` as an XPath 1.0 string literal, quotes and all.
///
/// XPath 1.0 has no escape mechanism inside a string literal, so a needle
/// containing both quote kinds cannot be written as one literal at all;
/// `concat()` over alternating pieces is the standard workaround.
///
/// Reaching the `concat()` branch means `s` carries both quote kinds, so
/// splitting on `"` yields at least two chunks and the loop pushes at least
/// one `'"'` separator alongside the chunk holding the apostrophe:
/// `concat()`'s two-argument minimum is satisfied by construction, not by a
/// guard.
///
/// This replaced `JSON.stringify(n).replace(/"/g,"'")`, which was not an
/// escaping strategy but a quote swap — it only moved which character
/// broke the expression. `text_exact("it's")` built
/// `//*[normalize-space(.)='it's']`, whose literal ends at the third
/// quote, so `document.evaluate` threw a `SyntaxError` and the caller got
/// a `JsException` instead of a match or an empty result. Double quotes
/// fared no better: `JSON.stringify` escaped an embedded `"` as `\"` and
/// the swap turned that into `\'`, which XPath 1.0 does not recognize.
fn xpath_string_literal(s: &str) -> String {
    if !s.contains('"') {
        return format!("\"{s}\"");
    }
    if !s.contains('\'') {
        return format!("'{s}'");
    }
    // Both kinds present. Splitting on `"` leaves pieces that contain
    // none, so each is safe inside double quotes; the separators are
    // re-introduced as single-quoted `"` characters.
    let chunks: Vec<&str> = s.split('"').collect();
    let mut parts: Vec<String> = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        if !chunk.is_empty() {
            parts.push(format!("\"{chunk}\""));
        }
        if i + 1 < chunks.len() {
            parts.push("'\"'".to_string());
        }
    }
    debug_assert!(parts.len() >= 2, "concat() needs two arguments: {parts:?}");
    format!("concat({})", parts.join(","))
}

/// Which node a text-exact expression walks down from.
///
/// `//` abbreviates `/descendant-or-self::node()/`, and that leading `/` is
/// the root of the document containing the context node — so `//*` ignores
/// whatever node `document.evaluate` was handed and enumerates the whole
/// page. `.//` starts the same walk at the context node. The distinction is
/// invisible until an element-scoped query returns a match from somewhere
/// else on the page.
#[derive(Clone, Copy)]
enum TextExactAnchor {
    /// Whole document. Tab and frame scope, evaluated against `document`.
    Document,
    /// The context node's own subtree. Element scope, evaluated against
    /// `this`.
    ContextNode,
}

impl TextExactAnchor {
    fn prefix(self) -> &'static str {
        match self {
            Self::Document => "//",
            Self::ContextNode => ".//",
        }
    }
}

/// The `//*[normalize-space(.)=<literal>]` expression for `needle`, or its
/// relative `.//*` form (see [`TextExactAnchor`]), with the needle rendered
/// by [`xpath_string_literal`]. Built here in Rust rather than assembled on
/// the page so the quoting is unit-testable without a browser.
fn text_exact_xpath(needle: &str, anchor: TextExactAnchor) -> String {
    format!(
        "{}*[normalize-space(.)={}]",
        anchor.prefix(),
        xpath_string_literal(needle)
    )
}

fn build_text_exact_xpath_js_tab(needle: &str, best_match: bool) -> String {
    // Snapshot form: returns an Array of every match. When `best_match` is
    // set the array is re-sorted by closest text length (see
    // `best_match_sort_js`).
    let sort = if best_match {
        format!(";{}", best_match_sort_js("a", needle.chars().count()))
    } else {
        String::new()
    };
    format!(
        "(function(){{var xp={xp};var r=document.evaluate(xp,document,null,XPathResult.ORDERED_NODE_SNAPSHOT_TYPE,null);var a=[];for(var i=0;i<r.snapshotLength;i++)a.push(r.snapshotItem(i));{sort};return a;}})()",
        xp = json!(text_exact_xpath(needle, TextExactAnchor::Document)),
        sort = sort,
    )
}

fn build_text_exact_xpath_fn_body(needle: &str, best_match: bool) -> String {
    // Element scope: `this` is the context node, so the expression has to be
    // relative or the walk runs over the whole document and the scope means
    // nothing. Declares no parameters — the needle is baked into the
    // expression, so the caller sends no `arguments` either.
    let sort = if best_match {
        format!(";{}", best_match_sort_js("a", needle.chars().count()))
    } else {
        String::new()
    };
    format!(
        "function(){{var xp={xp};var r=document.evaluate(xp,this,null,XPathResult.ORDERED_NODE_SNAPSHOT_TYPE,null);var a=[];for(var i=0;i<r.snapshotLength;i++)a.push(r.snapshotItem(i));{sort};return a;}}",
        xp = json!(text_exact_xpath(needle, TextExactAnchor::ContextNode)),
        sort = sort,
    )
}

async fn resolve_text_many(
    scope: &QueryScope<'_>,
    needle: &str,
    exact: bool,
    best_match: bool,
) -> Result<Vec<RemoteRef>> {
    // XPath compares `normalize-space(.)` against the needle, so the page side
    // was folded but the needle was not: `text_exact("Hello   world")` could
    // never match anything. Fold it the same way, using the one rule
    // `TextPred::Equals` uses so the two stay in agreement. Substring and regex
    // needles keep their raw text — interior whitespace can be meaningful there.
    let normalized;
    let needle = if exact {
        normalized = crate::query::predicate::normalize_space(needle);
        normalized.as_str()
    } else {
        needle
    };
    let session = scope.session();
    let result = if exact {
        match scope {
            QueryScope::Tab(_) | QueryScope::Frame(_) => {
                eval_expr_in_scope(scope, build_text_exact_xpath_js_tab(needle, best_match)).await?
            }
            QueryScope::Element(el) => {
                let object_id = el.remote_object_id_cloned().await?;
                session
                    .call(
                        "Runtime.callFunctionOn",
                        json!({
                            "objectId": object_id,
                            "functionDeclaration": build_text_exact_xpath_fn_body(needle, best_match),
                            "returnByValue": false,
                        }),
                    )
                    .await?
            }
        }
    } else {
        match scope {
            QueryScope::Tab(_) | QueryScope::Frame(_) => {
                eval_expr_in_scope(scope, build_text_substring_js_tab(needle, best_match)).await?
            }
            QueryScope::Element(el) => {
                let object_id = el.remote_object_id_cloned().await?;
                session
                    .call(
                        "Runtime.callFunctionOn",
                        json!({
                            "objectId": object_id,
                            "functionDeclaration": build_text_substring_fn_body(needle, best_match),
                            "arguments": [{ "value": needle }],
                            "returnByValue": false,
                        }),
                    )
                    .await?
            }
        }
    };
    extract_array_refs(session, &result).await
}

// ---------------------------------------------------------------------
// TextRegex (serialized as JS `new RegExp(pattern, flags)`)
// ---------------------------------------------------------------------
//
// JS path, with `r = new RegExp(pat, flags)`:
//   `Array.from(ctx.querySelectorAll('*')).filter(el => r.test(el.innerText||el.textContent))`,
//   then narrowed to the innermost matching element (see
//   `regex_narrowing_js`) — an ancestor whose only text-bearing
//   descendant is the match otherwise has an identical `innerText` and
//   also passes the filter, ranking first in document order.
//
// The page parses the pattern with `new RegExp`, so the pattern must use
// JS regex syntax, which differs from Rust's `regex` crate beyond the
// common subset (JS rejects `(?i)`, and reads `\z` as a literal `z`).
// Flags are passed verbatim — caller is responsible for valid JS flag
// chars (e.g. "i", "im", "gi", etc.). Empty flags string is fine.
//
// We construct the RegExp *once outside* the filter callback so the
// pattern is only compiled per query rather than per element. A `g` or `y`
// flag makes `test` start at, and then move, the RegExp's `lastIndex`, so
// every test sets it to 0 first; otherwise a match in one element would
// make the next element's test start partway through its text.

/// JS fragment implementing the same "narrowest match" filter the
/// substring builders use (see `build_text_substring_js_tab`), but with
/// the regex predicate (`r.test(t)`, reusing the already-constructed
/// `RegExp` `r`) in place of `.includes(lc)`. `matches_var` is the
/// in-scope identifier holding the raw (un-narrowed) candidate array;
/// the fragment declares `narrowed` as the result with any element that
/// has a descendant also matching `r` subtracted out. Shared between
/// `build_text_regex_js_tab` and `build_text_regex_fn_body` (kept
/// separate from the substring builders' fragment so their existing,
/// tested JS output stays untouched).
fn regex_narrowing_js(matches_var: &str) -> String {
    format!(
        "var narrowed={m}.filter(function(el){{\
            return !Array.from(el.querySelectorAll('*')).some(function(c){{\
                var t=c.innerText||c.textContent||'';\
                r.lastIndex=0;return r.test(t);\
            }});\
        }})",
        m = matches_var,
    )
}

fn build_text_regex_js_tab(pattern: &str, flags: &str, best_match: bool) -> String {
    // `best_match` has no literal needle for a regex; we use the pattern
    // length as the closest-length proxy (the only well-defined analog
    // for the regex case). The NARROWED array is re-sorted ascending by
    // `abs(len(text) - len(pattern))` when set, mirroring the substring
    // builder's order-of-operations (narrow first, then sort the
    // leaves).
    let sort = if best_match {
        format!(
            ";{}",
            best_match_sort_js("narrowed", pattern.chars().count())
        )
    } else {
        String::new()
    };
    format!(
        "(function(){{\
            var r=new RegExp({p}, {f});\
            var m=Array.from(document.querySelectorAll('*')).filter(function(el){{\
                var t=el.innerText||el.textContent||'';\
                r.lastIndex=0;return r.test(t);\
            }});\
            {narrowing}{sort};\
            return narrowed;\
        }})()",
        p = json!(pattern),
        f = json!(flags),
        narrowing = regex_narrowing_js("m"),
        sort = sort,
    )
}

fn build_text_regex_fn_body(pattern: &str, best_match: bool) -> String {
    // Element scope: `this` is the scope element. Pattern + flags
    // passed as arguments. See `build_text_regex_js_tab` for the
    // best_match proxy rationale and the narrowing semantics.
    let sort = if best_match {
        format!(
            ";{}",
            best_match_sort_js("narrowed", pattern.chars().count())
        )
    } else {
        String::new()
    };
    format!(
        "function(p,f){{\
            var r=new RegExp(p,f);\
            var m=Array.from(this.querySelectorAll('*')).filter(function(el){{\
                var t=el.innerText||el.textContent||'';\
                r.lastIndex=0;return r.test(t);\
            }});\
            {narrowing}{sort};\
            return narrowed;\
        }}",
        narrowing = regex_narrowing_js("m"),
        sort = sort,
    )
}

async fn resolve_text_regex_many(
    scope: &QueryScope<'_>,
    pattern: &str,
    flags: &str,
    best_match: bool,
) -> Result<Vec<RemoteRef>> {
    let session = scope.session();
    let result = match scope {
        QueryScope::Tab(_) | QueryScope::Frame(_) => {
            eval_expr_in_scope(scope, build_text_regex_js_tab(pattern, flags, best_match)).await?
        }
        QueryScope::Element(el) => {
            let object_id = el.remote_object_id_cloned().await?;
            session
                .call(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object_id,
                        "functionDeclaration": build_text_regex_fn_body(pattern, best_match),
                        "arguments": [{ "value": pattern }, { "value": flags }],
                        "returnByValue": false,
                    }),
                )
                .await?
        }
    };
    extract_array_refs(session, &result).await
}

// ---------------------------------------------------------------------
// Role (Chrome's accessibility tree)
// ---------------------------------------------------------------------

/// How long a role query waits for `Accessibility.queryAXTree`. Chrome
/// answers it only after the page next renders, which a background tab
/// doesn't do unless focus emulation is on (`StealthProfile::native` and
/// `spoofed` turn it on), so there the query would otherwise wait out the
/// transport's whole call budget.
const AX_QUERY_BUDGET: Duration = Duration::from_secs(10);

/// Every element under `scope` whose computed role is `role`, and, if
/// `name` is given, whose computed accessible name contains `name`
/// (case-insensitively), in the accessibility tree's order: document order,
/// except where `aria-owns` moves an element.
///
/// `Accessibility.queryAXTree` computes them, so an element matches by
/// the role it has implicitly (`<a href>` is a link, `<h1>` a heading) as
/// well as by a `role` attribute, and by a name that comes from
/// `aria-labelledby`, a `<label>` or its text. Like Playwright's
/// `getByRole`, it leaves out what the tree hides: an element under
/// `display: none` or `visibility: hidden`, and one that Chrome ignores
/// (under `aria-hidden="true"` or `inert`, or outside an open modal
/// `<dialog>`). It includes elements in shadow roots, closed ones too, and
/// doesn't descend into iframes. An Element scope whose element Chrome
/// leaves out of the tree (one with `role="presentation"` or `"none"`,
/// including a cell of a presentational table) finds nothing, because
/// Chrome's search starts from that element's place in the tree.
///
/// When Chrome doesn't answer within [`AX_QUERY_BUDGET`] (in a background
/// tab without focus emulation, under `StealthProfile::off`, say), this
/// fails with [`ZendriverError::CdpTimeout`] for
/// `Accessibility.queryAXTree`, and logs the likely causes. A match that
/// leaves the page before it is resolved is left out.
///
/// The scope's root (the document, or the element of an Element scope)
/// is an object in zendriver's isolated world, so each match is resolved
/// into the same world, whose context id the root's object id holds. An
/// Element scope's own element doesn't match, as with `querySelectorAll`.
async fn resolve_role_many(
    scope: &QueryScope<'_>,
    role: AriaRole,
    name: Option<&str>,
) -> Result<Vec<RemoteRef>> {
    let session = scope.session();
    let mut own_element = None;
    let root = match scope {
        QueryScope::Tab(_) | QueryScope::Frame(_) => {
            let reply = eval_expr_in_scope(scope, "document".to_string()).await?;
            let id = reply["result"]["objectId"].as_str().map(str::to_owned);
            id.ok_or_else(|| ZendriverError::Navigation("no object for the document".into()))?
        }
        QueryScope::Element(el) => {
            own_element = Some(el.backend_node_id_cloned().await?);
            el.remote_object_id_cloned().await?
        }
    };
    let context_id = crate::isolated_world::context_id_of_object(&root)?;
    let reply = session
        .connection()
        .call_raw_with_timeout(
            "Accessibility.queryAXTree",
            json!({ "objectId": root, "role": role.ax_name() }),
            session.session_id().map(str::to_owned),
            Some(AX_QUERY_BUDGET),
        )
        .await;
    if let Err(CallError::Timeout { .. }) = &reply {
        warn!(
            "Chrome's accessibility tree didn't answer a role query within {AX_QUERY_BUDGET:?}. \
             It answers only once the page renders again, which a background tab does only \
             with focus emulation (StealthProfile::native or spoofed; else activate the tab), \
             and a page that is navigating away, or an offscreen cross-origin iframe, may not."
        );
    }
    let reply = reply?;
    let needle = name.map(str::to_lowercase);

    let mut out = Vec::new();
    for node in reply["nodes"].as_array().into_iter().flatten() {
        if node["ignored"] == true {
            continue;
        }
        let Some(backend_node_id) = node["backendDOMNodeId"].as_i64() else {
            continue;
        };
        if Some(backend_node_id) == own_element {
            continue;
        }
        if let Some(needle) = &needle {
            let name = node["name"]["value"].as_str().unwrap_or_default();
            if !name.to_lowercase().contains(needle.as_str()) {
                continue;
            }
        }
        let resolved = session
            .call(
                "DOM.resolveNode",
                json!({ "backendNodeId": backend_node_id, "executionContextId": context_id }),
            )
            .await;
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(e) => {
                let e = ZendriverError::from(e);
                if crate::element::refresh::is_stale_node_error(&e) {
                    continue;
                }
                return Err(e);
            }
        };
        let Some(object_id) = resolved["object"]["objectId"].as_str() else {
            continue;
        };
        out.push(RemoteRef {
            remote_object_id: object_id.to_owned(),
            backend_node_id,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

/// Turn a `Runtime.evaluate` / `Runtime.callFunctionOn` reply whose
/// script returns a *single node* into a `RemoteRef`. `response` is what
/// the command returned (`{ result, exceptionDetails? }`), not its
/// `result` field, so a thrown exception fails with
/// [`ZendriverError::JsException`] instead of looking like no match. Null
/// subtype (`document.querySelector` returned `null`) or `undefined` =>
/// `Ok(None)`.
///
/// Takes a `SessionHandle` (not a `Tab`) so the follow-up
/// `DOM.describeNode` round-trip dispatches on the same session the
/// caller used to obtain `result` — Frame-scoped queries must keep the
/// follow-up on the Frame's session (which for OOPIFs is a distinct
/// child session from the parent tab's).
pub(crate) async fn extract_node_ref(
    session: &SessionHandle,
    response: &Value,
) -> Result<Option<RemoteRef>> {
    if let Some(e) = ZendriverError::js_exception_in(response) {
        return Err(e);
    }
    let result = &response["result"];
    if result["subtype"] == "null" || result["type"] == "undefined" {
        return Ok(None);
    }
    let Some(remote_object_id) = result["objectId"].as_str().map(str::to_string) else {
        return Ok(None);
    };
    let backend_node_id = describe_backend_id(session, &remote_object_id).await?;
    Ok(Some(RemoteRef {
        remote_object_id,
        backend_node_id,
    }))
}

/// Turn a `Runtime.evaluate` / `Runtime.callFunctionOn` reply whose
/// script returns an *array* of nodes into a `Vec<RemoteRef>` by
/// enumerating numeric properties via `Runtime.getProperties` and
/// describing each element node. `response` is what the command returned
/// (`{ result, exceptionDetails? }`), not its `result` field, so a thrown
/// exception (such as the `SyntaxError` of an invalid CSS selector
/// or regex) fails with [`ZendriverError::JsException`] instead of looking
/// like no match. Empty array yields an empty Vec, not an error.
///
/// See [`extract_node_ref`] for the rationale on taking a
/// `SessionHandle` rather than a `Tab` here.
pub(crate) async fn extract_array_refs(
    session: &SessionHandle,
    response: &Value,
) -> Result<Vec<RemoteRef>> {
    if let Some(e) = ZendriverError::js_exception_in(response) {
        return Err(e);
    }
    let result = &response["result"];
    if result["subtype"] == "null" || result["type"] == "undefined" {
        return Ok(Vec::new());
    }
    let Some(array_id) = result["objectId"].as_str() else {
        return Ok(Vec::new());
    };
    let props = session
        .call(
            "Runtime.getProperties",
            json!({
                "objectId": array_id,
                "ownProperties": true,
            }),
        )
        .await?;
    let entries = props["result"].as_array().cloned().unwrap_or_default();

    let mut out = Vec::new();
    for entry in entries {
        // Only numeric-indexed entries are array elements; "length",
        // proto, etc. are skipped here.
        let is_indexed = entry["name"]
            .as_str()
            .is_some_and(|n| n.parse::<usize>().is_ok());
        if !is_indexed {
            continue;
        }
        let value = &entry["value"];
        if value["subtype"] == "null" || value["type"] == "undefined" {
            continue;
        }
        if let Some(object_id) = value["objectId"].as_str().map(str::to_string) {
            let backend_node_id = describe_backend_id(session, &object_id).await?;
            out.push(RemoteRef {
                remote_object_id: object_id,
                backend_node_id,
            });
        }
    }
    // `out` is in array order: `Runtime.getProperties` lists own
    // properties in JavaScript's [[OwnPropertyKeys]] order, which puts an
    // array's indices first, ascending.
    Ok(out)
}

/// Read the rendered text length of `node` via `Runtime.callFunctionOn`
/// returning `(this.innerText||this.textContent||'').length`. Dispatched
/// on `scope`'s session so an OOPIF frame's node is read over the frame's
/// own session. Used only by the cross-scope `include_frames` +
/// `best_match` path to compare each scope's top candidate and pick the
/// global closest-length winner. A thrown exception (from a page-defined
/// `innerText` getter, say) fails with [`ZendriverError::JsException`], as
/// it does in the query itself. A missing/non-numeric result yields
/// `usize::MAX` so a scope whose length cannot be read never wins a tie.
pub(crate) async fn text_len_of(scope: &QueryScope<'_>, node: &RemoteRef) -> Result<usize> {
    let result = scope
        .session()
        .call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": node.remote_object_id,
                "functionDeclaration":
                    "function(){return (this.innerText||this.textContent||'').length;}",
                "returnByValue": true,
            }),
        )
        .await?;
    if let Some(e) = ZendriverError::js_exception_in(&result) {
        return Err(e);
    }
    Ok(result["result"]["value"]
        .as_u64()
        .map_or(usize::MAX, |v| v as usize))
}

async fn describe_backend_id(session: &SessionHandle, object_id: &str) -> Result<i64> {
    let described = session
        .call("DOM.describeNode", json!({ "objectId": object_id }))
        .await?;
    described["node"]["backendNodeId"].as_i64().ok_or_else(|| {
        ZendriverError::Navigation("DOM.describeNode returned no backendNodeId".into())
    })
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::test_support::{expect, expect_tab_query, serve_isolated_world};
    use zendriver_transport::SessionHandle;
    use zendriver_transport::testing::MockConnection;

    /// Drive the selector the way production does and hand back the JS that
    /// went out on the wire.
    ///
    /// Every resolver test goes through `resolve_many`, which is the only
    /// resolution entry point there is (`.one()` is this plus a take-first).
    /// The reply is a null subtype, which `extract_array_refs` short-circuits
    /// to an empty vec, so no `Runtime.getProperties` / `DOM.describeNode`
    /// dance is needed to inspect the dispatched expression.
    async fn dispatched_expression(kind: SelectorKind) -> String {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess);

        let fut = tokio::spawn({
            let t = tab.clone();
            async move {
                let scope = QueryScope::Tab(&t);
                kind.resolve_many(&scope).await
            }
        });

        let id = expect_tab_query(&mut mock).await;
        let expr = mock.last_sent()["params"]["expression"]
            .as_str()
            .unwrap()
            .to_string();
        mock.reply(
            id,
            json!({ "result": { "type": "object", "subtype": "null" } }),
        )
        .await;
        let hits = fut.await.unwrap().unwrap();
        assert!(hits.is_empty(), "null subtype must yield no matches");
        conn.shutdown();
        expr
    }

    /// A navigation can destroy the isolated world's context between its
    /// lookup and the query's `Runtime.evaluate`. The query must then look
    /// the world up again and retry once, rather than fail a `.one()` that
    /// is polling across the navigation.
    #[tokio::test]
    async fn a_query_retries_once_in_a_fresh_context_after_a_stale_one() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess);

        let fut = tokio::spawn({
            let t = tab.clone();
            async move {
                let scope = QueryScope::Tab(&t);
                SelectorKind::Css("p".into()).resolve_many(&scope).await
            }
        });

        let id = expect_tab_query(&mut mock).await;
        mock.reply_err(id, -32000, "Cannot find context with specified id")
            .await;

        serve_isolated_world(&mut mock, 6).await;
        let id = expect(&mut mock, "Runtime.evaluate").await;
        assert_eq!(mock.last_sent()["params"]["contextId"], 6);
        mock.reply(
            id,
            json!({ "result": { "type": "object", "subtype": "null" } }),
        )
        .await;

        assert!(fut.await.unwrap().unwrap().is_empty());
        conn.shutdown();
    }

    /// A query that matches nothing resolves to an empty vec, through the
    /// real decode path.
    ///
    /// `dispatched_expression` above short-circuits with a `null` subtype,
    /// which `extract_array_refs` early-returns on before it ever calls
    /// `Runtime.getProperties`. That is the right shortcut for tests whose
    /// job is the dispatched expression, but it leaves the ordinary
    /// empty-result path — a genuine array object, enumerated and found to
    /// hold nothing but `length` — unexercised. This covers it.
    #[tokio::test]
    async fn an_empty_result_array_resolves_to_no_matches() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess);

        let fut = tokio::spawn({
            let t = tab.clone();
            async move {
                let scope = QueryScope::Tab(&t);
                SelectorKind::Css("#absent".into())
                    .resolve_many(&scope)
                    .await
            }
        });

        let id_q = expect_tab_query(&mut mock).await;
        mock.reply(
            id_q,
            json!({ "result": { "objectId": "REmpty", "type": "object", "subtype": "array" } }),
        )
        .await;

        // A real (empty) array: `length` only, no numeric entries. The
        // resolver must enumerate it and stop, without a `DOM.describeNode`.
        let id_p = expect(&mut mock, "Runtime.getProperties").await;
        assert_eq!(mock.last_sent()["params"]["objectId"], "REmpty");
        mock.reply(
            id_p,
            json!({ "result": [{ "name": "length", "value": { "value": 0, "type": "number" } }] }),
        )
        .await;

        let hits = fut.await.unwrap().unwrap();
        assert!(hits.is_empty(), "an empty array must yield no matches");
        conn.shutdown();
    }

    /// A script that throws, such as `querySelectorAll` given an invalid
    /// selector, fails the resolve with the exception's description. The
    /// reply's `result` is then the thrown error object, which must not be
    /// enumerated as though it were the array of matches: an error has no
    /// numeric properties, so that would read as "no match".
    #[tokio::test]
    async fn a_thrown_exception_fails_the_resolve_with_its_description() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess);

        let fut = tokio::spawn({
            let t = tab.clone();
            async move {
                let scope = QueryScope::Tab(&t);
                SelectorKind::Css("div[".into()).resolve_many(&scope).await
            }
        });

        let id = expect_tab_query(&mut mock).await;
        let description = "SyntaxError: Failed to execute 'querySelectorAll' on 'Document': \
                           'div[' is not a valid selector.";
        let thrown = json!({
            "type": "object",
            "subtype": "error",
            "className": "DOMException",
            "description": description,
            "objectId": "RErr",
        });
        mock.reply(
            id,
            json!({
                "result": thrown,
                "exceptionDetails": { "exceptionId": 1, "text": "Uncaught", "exception": thrown },
            }),
        )
        .await;

        // Enumerating the error object would send `Runtime.getProperties`
        // and wait forever on a reply that this mock never sends.
        let err = tokio::time::timeout(std::time::Duration::from_secs(5), fut)
            .await
            .expect("the thrown error object must not be enumerated as matches")
            .unwrap()
            .expect_err("a thrown exception must fail the resolve");
        assert!(
            matches!(&err, ZendriverError::JsException(m) if m == description),
            "got {err:?}"
        );
        conn.shutdown();
    }

    /// Both extractors fail on a thrown exception before sending anything.
    /// The thrown error object in `result` has an `objectId`, so an
    /// extractor that took it for its node or array would send
    /// `DOM.describeNode` or `Runtime.getProperties` and wait forever on a
    /// reply that this mock never sends.
    #[tokio::test]
    async fn the_extractors_fail_on_a_thrown_exception_without_a_cdp_call() {
        let (_mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let thrown = json!({
            "type": "object",
            "subtype": "error",
            "description": "TypeError: boom",
            "objectId": "RErr",
        });
        let response = json!({ "result": thrown, "exceptionDetails": { "exception": thrown } });
        let budget = std::time::Duration::from_secs(5);

        let node = tokio::time::timeout(budget, extract_node_ref(&sess, &response))
            .await
            .expect("extract_node_ref must not describe the thrown error object");
        assert!(
            matches!(&node, Err(ZendriverError::JsException(m)) if m == "TypeError: boom"),
            "got {node:?}"
        );
        let array = tokio::time::timeout(budget, extract_array_refs(&sess, &response))
            .await
            .expect("extract_array_refs must not enumerate the thrown error object");
        assert!(
            matches!(&array, Err(ZendriverError::JsException(m)) if m == "TypeError: boom"),
            "got {array:?}"
        );
        conn.shutdown();
    }

    /// The XPath folds the page side with `normalize-space(.)`, so an
    /// author-written run of spaces in the needle could never match. Both
    /// sides must be folded by the same rule `TextPred::Equals` uses.
    ///
    /// This drives `resolve_many`, the path `.one()` and `.many()` both
    /// take. An earlier version of this test drove a single-match resolver
    /// that production never called, so deleting the fold from the shipping
    /// path left it green; that resolver is gone (see the module header).
    #[tokio::test]
    async fn text_exact_folds_whitespace_in_the_needle_before_building_the_xpath() {
        let expr = dispatched_expression(SelectorKind::Text {
            needle: "  Hello \t\n  world  ".into(),
            exact: true,
        })
        .await;

        // The whole expression the builder should emit, not a substring of
        // it: asserting the exact fragment pins the folding *and* the
        // quoting, and subsumes a negative check for surviving raw tabs.
        assert!(
            expr.contains(r#"var xp="//*[normalize-space(.)=\"Hello world\"]";"#),
            "needle should be normalize-space folded before embedding, got: {expr}"
        );
    }

    /// XPath 1.0 string literals have no escape mechanism, so a needle
    /// carrying a quote has to change how the literal is built rather than
    /// be escaped inside it. Quote-swapping used to produce
    /// `//*[normalize-space(.)='it's']`, which `document.evaluate` rejects
    /// with a `SyntaxError` — the caller saw a `JsException` instead of a
    /// match or an empty result, and apostrophes are ordinary in button and
    /// link text.
    #[tokio::test]
    async fn text_exact_needle_with_an_apostrophe_builds_a_valid_xpath_literal() {
        let expr = dispatched_expression(SelectorKind::Text {
            needle: "it's".into(),
            exact: true,
        })
        .await;

        assert!(
            expr.contains(r#"var xp="//*[normalize-space(.)=\"it's\"]";"#),
            "an apostrophe needle must switch the literal to double quotes, got: {expr}"
        );
    }

    /// The double-quote case was broken by the same swap for a different
    /// reason: `JSON.stringify` escaped the embedded `"` as `\"` and the
    /// replace turned that into `\'`, which XPath 1.0 does not recognize as
    /// an escape at all.
    #[tokio::test]
    async fn text_exact_needle_with_a_double_quote_builds_a_valid_xpath_literal() {
        let expr = dispatched_expression(SelectorKind::Text {
            needle: r#"say "hi""#.into(),
            exact: true,
        })
        .await;

        assert!(
            expr.contains(r#"var xp="//*[normalize-space(.)='say \"hi\"']";"#),
            "a double-quote needle must switch the literal to single quotes, got: {expr}"
        );
    }

    /// A needle carrying both quote kinds cannot be written as a single
    /// XPath literal, so the builder has to fall back to `concat()`.
    #[tokio::test]
    async fn text_exact_needle_with_both_quote_kinds_falls_back_to_concat() {
        let expr = dispatched_expression(SelectorKind::Text {
            needle: r#"it's a "quote""#.into(),
            exact: true,
        })
        .await;

        assert!(
            expr.contains(
                r#"var xp="//*[normalize-space(.)=concat(\"it's a \",'\"',\"quote\",'\"')]";"#
            ),
            "a needle with both quote kinds must compile to concat(), got: {expr}"
        );
    }

    /// The two text-exact builders must not share an anchor.
    ///
    /// `//*` resolves against the document root whatever context node
    /// `document.evaluate` is handed, so the element-scoped builder has to
    /// emit `.//*` or its scoping is decorative. Pinned here as well as in
    /// `tests/element_world_regressions.rs` so a mis-wired anchor fails
    /// without a browser; only that test proves the anchors reach the nodes
    /// this one assumes they do.
    #[test]
    fn only_the_element_scoped_expression_is_relative_to_its_context_node() {
        let tab_scope = build_text_exact_xpath_js_tab("Cancel", false);
        assert!(
            tab_scope.contains(r#"var xp="//*[normalize-space(.)=\"Cancel\"]""#),
            "tab scope evaluates against `document` and stays document-anchored, got: {tab_scope}"
        );

        let element_scope = build_text_exact_xpath_fn_body("Cancel", false);
        assert!(
            element_scope.contains(r#"var xp=".//*[normalize-space(.)=\"Cancel\"]""#),
            "element scope evaluates against `this` and must be relative, got: {element_scope}"
        );
    }

    #[tokio::test]
    async fn css_sends_query_selector_all_and_resolves_each_hit() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess);

        let fut = tokio::spawn({
            let t = tab.clone();
            async move {
                let scope = QueryScope::Tab(&t);
                SelectorKind::Css("#btn".into()).resolve_many(&scope).await
            }
        });

        let id_q = expect_tab_query(&mut mock).await;
        let sent = mock.last_sent()["params"]["expression"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            sent.contains("document.querySelectorAll") && sent.contains("#btn"),
            "expression should call document.querySelectorAll with the selector, got: {sent}"
        );
        mock.reply(
            id_q,
            json!({ "result": { "objectId": "RArr", "type": "object", "subtype": "array" } }),
        )
        .await;

        let id_p = expect(&mut mock, "Runtime.getProperties").await;
        assert_eq!(mock.last_sent()["params"]["objectId"], "RArr");
        mock.reply(
            id_p,
            json!({
                "result": [
                    { "name": "0", "value": { "objectId": "R7", "type": "object", "subtype": "node" } },
                    { "name": "length", "value": { "value": 1, "type": "number" } }
                ]
            }),
        )
        .await;

        let id_d = expect(&mut mock, "DOM.describeNode").await;
        assert_eq!(mock.last_sent()["params"]["objectId"], "R7");
        mock.reply(id_d, json!({ "node": { "backendNodeId": 99 } }))
            .await;

        let hits = fut.await.unwrap().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].remote_object_id, "R7");
        assert_eq!(hits[0].backend_node_id, 99);
        conn.shutdown();
    }

    /// Non-exact text selector: the dispatched JS must carry the
    /// lowercase-fold and the needle verbatim, so the case-insensitive
    /// substring contract survives. Unlike the exact path, the needle is
    /// *not* whitespace-folded — interior runs can be meaningful in a
    /// substring.
    #[tokio::test]
    async fn text_substring_eval_lowercases_and_includes_needle() {
        let expr = dispatched_expression(SelectorKind::Text {
            needle: "Sign In".into(),
            exact: false,
        })
        .await;

        assert!(
            expr.contains(".toLowerCase()"),
            "substring path must lowercase-fold both sides; got: {expr}"
        );
        assert!(
            expr.contains("Sign In"),
            "substring path must embed the needle verbatim; got: {expr}"
        );
        assert!(
            expr.contains(".includes("),
            "substring path must call .includes; got: {expr}"
        );
    }

    /// TextRegex selector: the dispatched JS builds `new RegExp(<pat>,
    /// <flags>)` with both strings present.
    #[tokio::test]
    async fn text_regex_eval_constructs_new_regexp_with_pattern_and_flags() {
        let expr = dispatched_expression(SelectorKind::TextRegex {
            pattern: "hello.*world".into(),
            flags: "im".into(),
        })
        .await;

        assert!(
            expr.contains("new RegExp"),
            "regex path must instantiate `new RegExp`; got: {expr}"
        );
        assert!(
            expr.contains("hello.*world"),
            "regex path must embed the pattern; got: {expr}"
        );
        assert!(
            expr.contains("im"),
            "regex path must embed the flags string; got: {expr}"
        );
    }

    /// Regression test: `.text_regex()` used to have no "narrowest match"
    /// step (unlike `.text()` / `.text_exact()`), so an ancestor
    /// (`<html>` / `<body>`) whose only text-bearing descendant is the real
    /// match ends up with an identical `innerText`, also passes the regex
    /// filter, ranks first in document order, and wins `.one()`'s `[0]`
    /// instead of the intended leaf. The dispatched expression must
    /// subtract any element that has a matching descendant (mirroring
    /// `build_text_substring_js_tab`'s narrowing), reusing the same
    /// `RegExp` object as the predicate.
    #[tokio::test]
    async fn text_regex_eval_narrows_to_innermost_matching_element() {
        let expr = dispatched_expression(SelectorKind::TextRegex {
            pattern: "unique-frame-text".into(),
            flags: "".into(),
        })
        .await;

        assert!(
            expr.contains(".querySelectorAll('*')).some("),
            "regex path must narrow via descendant-subtraction (some()); got: {expr}"
        );
        assert!(
            expr.matches("r.test(").count() >= 2,
            "narrowing predicate must reuse the same RegExp object `r` (once for the initial \
             filter, once for the descendant check); got: {expr}"
        );
    }

    /// A role query asks Chrome's accessibility tree for the role under the
    /// scope's `document`, skips the nodes Chrome ignores and those whose
    /// name doesn't contain the needle (case-insensitively), and resolves
    /// each match into the world of the `document` object, whose context id
    /// that object's id holds.
    #[tokio::test]
    async fn role_queries_the_ax_tree_and_resolves_matches_into_the_isolated_world() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess);

        let fut = tokio::spawn({
            let t = tab.clone();
            async move {
                let scope = QueryScope::Tab(&t);
                SelectorKind::Role(AriaRole::Button, Some("SAVE".into()))
                    .resolve_many(&scope)
                    .await
            }
        });

        let id = expect_tab_query(&mut mock).await;
        assert_eq!(mock.last_sent()["params"]["expression"], "document");
        mock.reply(id, json!({ "result": { "objectId": "9.7.1" } }))
            .await;
        let id = expect(&mut mock, "Accessibility.queryAXTree").await;
        assert_eq!(mock.last_sent()["params"]["objectId"], "9.7.1");
        assert_eq!(mock.last_sent()["params"]["role"], "button");
        let nodes = json!({ "nodes": [
            { "ignored": false, "backendDOMNodeId": 11, "name": { "value": "Save draft" } },
            { "ignored": true, "backendDOMNodeId": 12, "name": { "value": "Save ignored" } },
            { "ignored": false, "backendDOMNodeId": 13, "name": { "value": "Cancel" } },
            { "ignored": false, "backendDOMNodeId": 14, "name": { "value": "Autosave" } },
        ] });
        mock.reply(id, nodes).await;
        for (backend, object) in [(11, "9.7.2"), (14, "9.7.3")] {
            let id = expect(&mut mock, "DOM.resolveNode").await;
            assert_eq!(mock.last_sent()["params"]["backendNodeId"], backend);
            assert_eq!(mock.last_sent()["params"]["executionContextId"], 7);
            mock.reply(id, json!({ "object": { "objectId": object } }))
                .await;
        }

        let hits = fut.await.unwrap().unwrap();
        let hits: Vec<(&str, i64)> = hits
            .iter()
            .map(|h| (h.remote_object_id.as_str(), h.backend_node_id))
            .collect();
        assert_eq!(hits, [("9.7.2", 11), ("9.7.3", 14)]);
        conn.shutdown();
    }

    /// In an Element scope the query's root is the element itself, which
    /// doesn't match, as with `querySelectorAll`.
    #[tokio::test]
    async fn role_in_an_element_scope_leaves_out_the_element() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess);
        let el = Element::from_jsret(tab, 21, "9.7.5".to_string());

        let fut = tokio::spawn(async move {
            let scope = QueryScope::Element(&el);
            SelectorKind::Role(AriaRole::List, None)
                .resolve_many(&scope)
                .await
        });

        let id = expect(&mut mock, "Accessibility.queryAXTree").await;
        assert_eq!(mock.last_sent()["params"]["objectId"], "9.7.5");
        let nodes = json!({ "nodes": [
            { "ignored": false, "backendDOMNodeId": 21 },
            { "ignored": false, "backendDOMNodeId": 22 },
        ] });
        mock.reply(id, nodes).await;
        let id = expect(&mut mock, "DOM.resolveNode").await;
        assert_eq!(mock.last_sent()["params"]["backendNodeId"], 22);
        mock.reply(id, json!({ "object": { "objectId": "9.7.6" } }))
            .await;

        let hits = fut.await.unwrap().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].backend_node_id, 22);
        conn.shutdown();
    }

    // -------------------------------------------------------------------
    // Frame-scope `contextId` pinning (regression coverage for the bug
    // where xpath/text/text_regex `_many` resolvers silently queried the
    // main-frame document instead of the scoped frame's).
    // -------------------------------------------------------------------

    /// Build a synthetic `Frame` whose session sits on the supplied mock
    /// connection, with no parent tab/frame — sufficient for exercising
    /// `QueryScope::Frame` dispatch without a real `Tab`.
    fn frame_on(session: SessionHandle, frame_id: &str) -> Frame {
        Frame::new(
            frame_id.to_string(),
            None,
            String::new(),
            None,
            session,
            std::sync::Weak::new(),
        )
    }

    #[tokio::test]
    async fn xpath_many_frame_scope_pins_context_id() {
        // Frame scope must allocate/reuse the frame's isolated-world
        // context via `Page.createIsolatedWorld` and pin the follow-up
        // `Runtime.evaluate` to it via `contextId` — otherwise
        // `document.evaluate(...)` inside the expression walks the
        // parent tab's document instead of the frame's.
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = frame_on(sess, "F1");

        let fut = tokio::spawn(async move {
            let scope = QueryScope::Frame(&frame);
            SelectorKind::Xpath("//button".into())
                .resolve_many(&scope)
                .await
        });

        let id_iso = mock.expect_cmd("Page.createIsolatedWorld").await;
        assert_eq!(mock.last_sent()["params"]["frameId"], "F1");
        mock.reply(id_iso, json!({ "executionContextId": 777 }))
            .await;

        let id_q = mock.expect_cmd("Runtime.evaluate").await;
        assert_eq!(
            mock.last_sent()["params"]["contextId"],
            777,
            "xpath_many must pin Runtime.evaluate to the frame's isolated-world contextId"
        );
        mock.reply(
            id_q,
            json!({ "result": { "type": "object", "subtype": "null" } }),
        )
        .await;

        let r = fut.await.unwrap().unwrap();
        assert!(r.is_empty(), "null subtype must yield an empty Vec");
        conn.shutdown();
    }

    #[tokio::test]
    async fn text_many_frame_scope_pins_context_id() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = frame_on(sess, "F1");

        let fut = tokio::spawn(async move {
            let scope = QueryScope::Frame(&frame);
            SelectorKind::Text {
                needle: "hello".into(),
                exact: false,
            }
            .resolve_many(&scope)
            .await
        });

        let id_iso = mock.expect_cmd("Page.createIsolatedWorld").await;
        assert_eq!(mock.last_sent()["params"]["frameId"], "F1");
        mock.reply(id_iso, json!({ "executionContextId": 778 }))
            .await;

        let id_q = mock.expect_cmd("Runtime.evaluate").await;
        assert_eq!(
            mock.last_sent()["params"]["contextId"],
            778,
            "text_many must pin Runtime.evaluate to the frame's isolated-world contextId"
        );
        mock.reply(
            id_q,
            json!({ "result": { "type": "object", "subtype": "null" } }),
        )
        .await;

        let r = fut.await.unwrap().unwrap();
        assert!(r.is_empty(), "null subtype must yield an empty Vec");
        conn.shutdown();
    }

    #[tokio::test]
    async fn text_regex_many_frame_scope_pins_context_id() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = frame_on(sess, "F1");

        let fut = tokio::spawn(async move {
            let scope = QueryScope::Frame(&frame);
            SelectorKind::TextRegex {
                pattern: "hello.*world".into(),
                flags: "i".into(),
            }
            .resolve_many(&scope)
            .await
        });

        let id_iso = mock.expect_cmd("Page.createIsolatedWorld").await;
        assert_eq!(mock.last_sent()["params"]["frameId"], "F1");
        mock.reply(id_iso, json!({ "executionContextId": 779 }))
            .await;

        let id_q = mock.expect_cmd("Runtime.evaluate").await;
        assert_eq!(
            mock.last_sent()["params"]["contextId"],
            779,
            "text_regex_many must pin Runtime.evaluate to the frame's isolated-world contextId"
        );
        mock.reply(
            id_q,
            json!({ "result": { "type": "object", "subtype": "null" } }),
        )
        .await;

        let r = fut.await.unwrap().unwrap();
        assert!(r.is_empty(), "null subtype must yield an empty Vec");
        conn.shutdown();
    }

    #[tokio::test]
    async fn role_many_frame_scope_queries_the_frames_document() {
        // A Frame-scoped role query must root the accessibility query at
        // the frame's own document, found in the frame's isolated world,
        // not at the parent tab's.
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let frame = frame_on(sess, "F1");

        let fut = tokio::spawn(async move {
            let scope = QueryScope::Frame(&frame);
            SelectorKind::Role(AriaRole::Button, None)
                .resolve_many(&scope)
                .await
        });

        let id_iso = mock.expect_cmd("Page.createIsolatedWorld").await;
        assert_eq!(mock.last_sent()["params"]["frameId"], "F1");
        mock.reply(id_iso, json!({ "executionContextId": 780 }))
            .await;

        let id_q = mock.expect_cmd("Runtime.evaluate").await;
        assert_eq!(
            mock.last_sent()["params"]["contextId"],
            780,
            "role_many must find the document in the frame's isolated world"
        );
        mock.reply(id_q, json!({ "result": { "objectId": "9.780.1" } }))
            .await;
        let id_ax = mock.expect_cmd("Accessibility.queryAXTree").await;
        assert_eq!(mock.last_sent()["params"]["objectId"], "9.780.1");
        mock.reply(id_ax, json!({ "nodes": [] })).await;

        let r = fut.await.unwrap().unwrap();
        assert!(r.is_empty(), "no AX nodes must yield an empty Vec");
        conn.shutdown();
    }
}
