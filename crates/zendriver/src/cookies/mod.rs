//! Cookie stores of browser contexts, backed by CDP `Storage.*Cookies`
//! methods.
//!
//! [`CookieJar`] wraps a [`Connection`] and exposes ergonomic CRUD over
//! Chrome's cookie store. The jar is cheap to clone — internally an `Arc`
//! over a thin inner struct — so it can be passed around freely.
//!
//! A jar reads and writes the cookie store of one `BrowserContext`: the
//! default one ([`CookieJar::new`], via [`crate::Browser::cookies`]), or a
//! given one ([`CookieJar::for_context`], via [`crate::Tab::cookies`], which
//! names the tab's own context). The two are the same store unless a tab
//! was opened in an isolated context ([`crate::BrowserContext`]).
//!
//! ```no_run
//! # async fn ex() -> zendriver::Result<()> {
//! # let browser = zendriver::Browser::builder().launch().await?;
//! let jar = browser.cookies();
//! jar.set(zendriver::Cookie {
//!     name: "sid".into(),
//!     value: "abc123".into(),
//!     domain: ".example.com".into(),
//!     path: "/".into(),
//!     http_only: true,
//!     secure: true,
//!     same_site: Some(zendriver::SameSite::Lax),
//!     ..Default::default()
//! }).await?;
//! # Ok(()) }
//! ```
//!
//! ## Serialization — snake_case on disk, camelCase on the wire
//!
//! The public [`Cookie`] struct uses idiomatic snake_case for JSON output
//! (so users' on-disk JSON looks Rust-natural), while a private internal
//! mirror handles the CDP camelCase rename. Lossless conversion in both
//! directions; users see clean snake_case JSON, CDP sees camelCase, neither
//! side has to know the other exists.

pub mod persistence;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use zendriver_transport::Connection;

use crate::error::{Result, ZendriverError};

/// SameSite policy as defined by RFC 6265bis.
///
/// Mirrors the CDP `Network.CookieSameSite` enum. Serializes as
/// `"Strict"` / `"Lax"` / `"None"` to match CDP exactly (and the
/// standard's capitalization).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SameSite {
    /// First-party only — never sent on cross-site requests.
    Strict,
    /// Sent on cross-site top-level navigations only (the default
    /// modern-browser behavior for unspecified SameSite).
    Lax,
    /// Always sent, including in third-party contexts. Requires `Secure`.
    None,
}

/// Cookie priority hint as defined by the (non-standard) `Priority`
/// attribute Chrome honors during eviction.
///
/// Mirrors the CDP `Network.CookiePriority` enum. Serializes as
/// `"Low"` / `"Medium"` / `"High"` to match CDP exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CookiePriority {
    /// Lowest retention priority — evicted first under storage pressure.
    Low,
    /// Default priority when the attribute is unspecified.
    Medium,
    /// Highest retention priority — evicted last.
    High,
}

/// Scheme of the request that set the cookie.
///
/// Mirrors the CDP `Network.CookieSourceScheme` enum. Serializes as
/// `"Unset"` / `"NonSecure"` / `"Secure"` to match CDP exactly. CDP uses
/// this together with `secure` to implement the "schemeful same-site"
/// model; `Unset` is the default for cookies set without a known scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CookieSourceScheme {
    /// Scheme unknown / not recorded.
    Unset,
    /// Cookie was set over a non-secure (`http`) origin.
    NonSecure,
    /// Cookie was set over a secure (`https`) origin.
    Secure,
}

/// CHIPS partition key — the top-level site a partitioned cookie is scoped
/// to, plus whether it was set from a cross-site context. Mirrors CDP's
/// `Network.CookiePartitionKey` (M119+).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CookiePartitionKey {
    /// The top-level site (e.g. `"https://example.com"`).
    pub top_level_site: String,
    /// `true` if the cookie was set from a cross-site context.
    #[serde(default)]
    pub has_cross_site_ancestor: bool,
}

impl CookiePartitionKey {
    /// Partition key for `top_level_site` with `has_cross_site_ancestor = false`
    /// (the common case).
    pub fn new(top_level_site: impl Into<String>) -> Self {
        Self {
            top_level_site: top_level_site.into(),
            has_cross_site_ancestor: false,
        }
    }
}

impl From<&str> for CookiePartitionKey {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for CookiePartitionKey {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

/// A single HTTP cookie.
///
/// Field shape matches the public Rust/JSON contract (snake_case). An
/// internal mirror handles the CDP camelCase rename — see the module-level
/// docs.
///
/// - `expires` is a Unix-epoch timestamp in **seconds** (with sub-second
///   precision), matching CDP's `Network.TimeSinceEpoch`. `None` means
///   "session cookie" (deleted when the browser closes).
/// - `url` is a constructor-time convenience for [`CookieJar::set`]: when
///   present, CDP infers `domain` + `path` + `secure` from it. CDP never
///   emits this field on reads, so it's always `None` on cookies returned
///   by [`CookieJar::all`] / [`CookieJar::for_url`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Cookie {
    /// Cookie name.
    pub name: String,
    /// Cookie value.
    pub value: String,
    /// Domain the cookie applies to. Leading dot (e.g. `.example.com`)
    /// matches the domain and all subdomains.
    pub domain: String,
    /// URL path the cookie applies to (typically `"/"`).
    pub path: String,
    /// Expiration timestamp in seconds since Unix epoch. `None` for a
    /// session cookie (deleted when the browser closes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
    /// `true` if the cookie has the `HttpOnly` flag (not accessible via
    /// JavaScript).
    #[serde(default)]
    pub http_only: bool,
    /// `true` if the cookie is only sent over HTTPS.
    #[serde(default)]
    pub secure: bool,
    /// SameSite policy, if specified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<SameSite>,
    /// Origin URL for [`CookieJar::set`] convenience — CDP infers
    /// `domain` + `path` + `secure` from it. Always `None` on cookies
    /// returned by reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Eviction-priority hint (`Network.CookiePriority`). `None` lets
    /// Chrome apply its default (`Medium`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<CookiePriority>,
    /// First-Party Sets membership flag (`sameParty`). Rarely set by
    /// hand; populated by CDP on reads when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_party: Option<bool>,
    /// Scheme of the origin that set the cookie
    /// (`Network.CookieSourceScheme`). Drives schemeful same-site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_scheme: Option<CookieSourceScheme>,
    /// Source port of the origin that set the cookie. `-1` in CDP means
    /// "unspecified"; modeled as a plain `Option<i32>` here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_port: Option<i32>,
    /// CHIPS partition key — the top-level site the partitioned cookie is
    /// scoped to, plus whether it was set from a cross-site context.
    /// Mirrors CDP's `Network.CookiePartitionKey` (M119+).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition_key: Option<CookiePartitionKey>,
}

impl Cookie {
    /// Whether a request to `url` would carry this cookie, by the rules
    /// that [`CookieJar::for_url`] lists.
    fn matches_url(&self, url: &url::Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let domain = self.domain.to_ascii_lowercase();
        let domain_matches = match domain.strip_prefix('.') {
            Some(parent) => {
                host == parent
                    || host
                        .strip_suffix(parent)
                        .is_some_and(|sub| sub.ends_with('.'))
            }
            None => host == domain,
        };
        let request_path = url.path();
        let path_matches = request_path == self.path
            || request_path
                .strip_prefix(self.path.as_str())
                .is_some_and(|rest| self.path.ends_with('/') || rest.starts_with('/'));
        domain_matches && path_matches && (!self.secure || is_secure(url))
    }
}

/// Whether Chrome sends `Secure` cookies to `url`: a secure scheme, or a
/// loopback host.
fn is_secure(url: &url::Url) -> bool {
    if matches!(url.scheme(), "https" | "wss") {
        return true;
    }
    match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost" || d.ends_with(".localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// The `expires` that [`CookieJar::delete`] gives a cookie: one second
/// into the Unix epoch, long past.
const EXPIRED: f64 = 1.0;

/// Internal mirror of [`Cookie`] with CDP's camelCase rename. Used only at
/// the transport boundary — never escapes this module. Round-trips losslessly
/// through `From<Cookie>` / `From<CdpCookie>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CdpCookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires: Option<f64>,
    #[serde(default)]
    http_only: bool,
    #[serde(default)]
    secure: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    same_site: Option<SameSite>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    priority: Option<CookiePriority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    same_party: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_scheme: Option<CookieSourceScheme>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_port: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    partition_key: Option<CookiePartitionKey>,
}

impl From<Cookie> for CdpCookie {
    fn from(c: Cookie) -> Self {
        Self {
            name: c.name,
            value: c.value,
            domain: c.domain,
            path: c.path,
            expires: c.expires,
            http_only: c.http_only,
            secure: c.secure,
            same_site: c.same_site,
            url: c.url,
            priority: c.priority,
            same_party: c.same_party,
            source_scheme: c.source_scheme,
            source_port: c.source_port,
            partition_key: c.partition_key,
        }
    }
}

impl From<CdpCookie> for Cookie {
    fn from(c: CdpCookie) -> Self {
        Self {
            name: c.name,
            value: c.value,
            domain: c.domain,
            path: c.path,
            expires: c.expires,
            http_only: c.http_only,
            secure: c.secure,
            same_site: c.same_site,
            url: c.url,
            priority: c.priority,
            same_party: c.same_party,
            source_scheme: c.source_scheme,
            source_port: c.source_port,
            partition_key: c.partition_key,
        }
    }
}

/// Cheap-to-clone handle to a Chrome cookie store.
///
/// Wraps a [`Connection`] in an [`Arc`]; cloning is reference-bump cheap.
///
/// ## Which cookie store
///
/// Each `BrowserContext` has its own cookie store. A jar sends every command
/// to the browser target (no `sessionId`), naming the context with
/// `browserContextId`:
///
/// - [`CookieJar::new`]: no `browserContextId`, so the **default** context.
///   This is what [`crate::Browser::cookies`] returns.
/// - [`CookieJar::for_context`]: the named context. [`crate::Tab::cookies`]
///   returns one for the tab's own context.
///
/// Chrome accepts `browserContextId` only on the browser target, and a
/// `Storage.*Cookies` command sent on a page's session reads the default
/// context whatever page sent it, so the connection must reach the browser
/// target.
#[derive(Clone, Debug)]
pub struct CookieJar {
    inner: Arc<CookieJarInner>,
}

#[derive(Debug)]
struct CookieJarInner {
    /// A connection whose session-less commands reach the browser target.
    conn: Connection,
    /// The `BrowserContext` whose cookies the jar reads and writes; `None`
    /// is the default context.
    browser_context_id: Option<String>,
}

impl CookieJar {
    /// Construct a jar for the **default** `BrowserContext`.
    ///
    /// `conn` must reach the browser target with session-less commands.
    /// Typically called by [`crate::Browser::cookies`] rather than user code.
    #[must_use]
    pub fn new(conn: Connection) -> Self {
        Self {
            inner: Arc::new(CookieJarInner {
                conn,
                browser_context_id: None,
            }),
        }
    }

    /// Construct a jar for the `BrowserContext` whose id is
    /// `browser_context_id` (as in CDP's `Target.TargetInfo`).
    ///
    /// `conn` must reach the browser target with session-less commands.
    /// Typically called by [`crate::Tab::cookies`] rather than user code.
    #[must_use]
    pub fn for_context(conn: Connection, browser_context_id: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(CookieJarInner {
                conn,
                browser_context_id: Some(browser_context_id.into()),
            }),
        }
    }

    /// Send the `Storage.*` command `method` to the browser target, with
    /// `params` plus this jar's `browserContextId`, and return the reply.
    async fn call(&self, method: &str, mut params: serde_json::Value) -> Result<serde_json::Value> {
        if let Some(id) = &self.inner.browser_context_id {
            params["browserContextId"] = json!(id);
        }
        Ok(self.inner.conn.call_raw(method, params, None).await?)
    }

    /// Return every cookie in the jar's context.
    ///
    /// Maps to `Storage.getCookies`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// let jar = browser.cookies();
    /// for c in jar.all().await? {
    ///     println!("{}={} ({})", c.name, c.value, c.domain);
    /// }
    /// # Ok(()) }
    /// ```
    pub async fn all(&self) -> Result<Vec<Cookie>> {
        let resp = self.call("Storage.getCookies", json!({})).await?;
        parse_cookies(&resp)
    }

    /// Return the cookies that a request to `url` would carry.
    ///
    /// Reads every cookie with `Storage.getCookies` and keeps those whose
    /// domain, path and `Secure` flag match `url` by RFC 6265's rules
    /// (section 5.4):
    ///
    /// - Domain: a cookie whose domain starts with a dot matches that
    ///   domain and its subdomains; any other matches exactly that host.
    /// - Path: the URL's path is the cookie's path, or below it.
    /// - `Secure`: only for a secure URL: `https`, `wss`, or a loopback
    ///   host (`localhost`, `*.localhost`, `127.0.0.0/8`, `[::1]`), which
    ///   Chrome treats as secure.
    ///
    /// `SameSite` and the partition key are ignored: they depend on the
    /// page making the request. Chrome's own `Network.getCookies` isn't
    /// used because the browser target has no `Network` domain. Accepting
    /// a parsed [`url::Url`] surfaces malformed-URL errors at construction
    /// time instead of as a confusing empty result.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// let jar = browser.cookies();
    /// let url = url::Url::parse("https://example.com/").unwrap();
    /// let cookies = jar.for_url(&url).await?;
    /// # let _ = cookies;
    /// # Ok(()) }
    /// ```
    pub async fn for_url(&self, url: &url::Url) -> Result<Vec<Cookie>> {
        let mut cookies = self.all().await?;
        cookies.retain(|c| c.matches_url(url));
        Ok(cookies)
    }

    /// Set a single cookie.
    ///
    /// Maps to `Storage.setCookies` with a one-element batch. The singular
    /// `Network.setCookie` is removed from newer Chrome / Chromium builds
    /// (the CDP method is reported as not found), so the bulk endpoint is
    /// the portable choice and is what the parent zendriver-python project
    /// uses too.
    ///
    /// # Errors
    ///
    /// Returns [`ZendriverError::Cookie`] if CDP rejects the cookie.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zendriver::{Cookie, SameSite};
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// browser.cookies().set(Cookie {
    ///     name: "sid".into(),
    ///     value: "abc".into(),
    ///     domain: ".example.com".into(),
    ///     path: "/".into(),
    ///     http_only: true,
    ///     secure: true,
    ///     same_site: Some(SameSite::Lax),
    ///     ..Default::default()
    /// }).await?;
    /// # Ok(()) }
    /// ```
    pub async fn set(&self, cookie: Cookie) -> Result<()> {
        let cdp: CdpCookie = cookie.into();
        let cdp_json = serde_json::to_value(&cdp).map_err(ZendriverError::Serde)?;
        self.call("Storage.setCookies", json!({ "cookies": [cdp_json] }))
            .await?;
        Ok(())
    }

    /// Set many cookies in a single CDP call.
    ///
    /// Maps to `Storage.setCookies`. Faster than looping over [`Self::set`]
    /// for bulk loads (one round-trip instead of N).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zendriver::Cookie;
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// let cookies = vec![
    ///     Cookie { name: "a".into(), value: "1".into(), domain: ".x.com".into(),
    ///              path: "/".into(), ..Default::default() },
    /// ];
    /// browser.cookies().set_many(cookies).await?;
    /// # Ok(()) }
    /// ```
    pub async fn set_many(&self, cookies: Vec<Cookie>) -> Result<()> {
        let cdp: Vec<CdpCookie> = cookies.into_iter().map(CdpCookie::from).collect();
        self.call("Storage.setCookies", json!({ "cookies": cdp }))
            .await?;
        Ok(())
    }

    /// Delete the cookies named `name`, narrowed to those whose domain is
    /// `domain` and whose path is `path` when given. Without `domain` and
    /// `path`, deletes every cookie named `name`, on every domain and path.
    ///
    /// `domain` is compared ignoring a leading dot on either side, so
    /// `"example.com"` matches both a cookie that only `example.com`
    /// receives and a `.example.com` cookie that its subdomains receive.
    ///
    /// Reads every cookie with `Storage.getCookies`, then overwrites each
    /// match with an expired copy through `Storage.setCookies`, which makes
    /// Chrome drop it. Chrome's `Network.deleteCookies` isn't used because
    /// the browser target has no `Network` domain, and it can't delete by
    /// name alone. Deleting a cookie that doesn't exist is not an error.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// browser.cookies().delete("sid", Some(".example.com"), Some("/")).await?;
    /// # Ok(()) }
    /// ```
    pub async fn delete(&self, name: &str, domain: Option<&str>, path: Option<&str>) -> Result<()> {
        let domain = domain.map(|d| d.trim_start_matches('.'));
        let expired: Vec<CdpCookie> = self
            .all()
            .await?
            .into_iter()
            .filter(|c| {
                c.name == name
                    && domain.is_none_or(|d| c.domain.trim_start_matches('.') == d)
                    && path.is_none_or(|p| c.path == p)
            })
            .map(|c| CdpCookie {
                expires: Some(EXPIRED),
                ..c.into()
            })
            .collect();
        if expired.is_empty() {
            return Ok(());
        }
        self.call("Storage.setCookies", json!({ "cookies": expired }))
            .await?;
        Ok(())
    }

    /// Delete every cookie in the jar's context.
    ///
    /// Maps to `Storage.clearCookies`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// browser.cookies().clear().await?;
    /// # Ok(()) }
    /// ```
    pub async fn clear(&self) -> Result<()> {
        self.call("Storage.clearCookies", json!({})).await?;
        Ok(())
    }
}

/// Parser for the `{ cookies: CdpCookie[] }` that `Storage.getCookies`
/// returns.
#[allow(clippy::result_large_err)] // ZendriverError variance is the project-wide return type
fn parse_cookies(resp: &serde_json::Value) -> Result<Vec<Cookie>> {
    let arr = resp
        .get("cookies")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ZendriverError::Cookie("response missing `cookies` array".into()))?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let cdp: CdpCookie = serde_json::from_value(v.clone()).map_err(ZendriverError::Serde)?;
        out.push(cdp.into());
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use zendriver_transport::testing::MockConnection;

    /// [`CookieJar::all`] dispatches `Storage.getCookies` and parses the
    /// `cookies` array (with camelCase fields) into snake_case [`Cookie`]s.
    #[tokio::test]
    async fn all_parses_get_all_cookies_response() {
        let (mut mock, conn) = MockConnection::pair();
        let jar = CookieJar::new(conn.clone());

        let call = tokio::spawn({
            let j = jar.clone();
            async move { j.all().await }
        });

        let id = mock.expect_cmd("Storage.getCookies").await;
        mock.reply(
            id,
            json!({
                "cookies": [
                    {
                        "name": "sid",
                        "value": "abc",
                        "domain": ".example.com",
                        "path": "/",
                        "expires": 1_700_000_000.5,
                        "httpOnly": true,
                        "secure": true,
                        "sameSite": "Lax",
                    },
                    {
                        "name": "theme",
                        "value": "dark",
                        "domain": "example.com",
                        "path": "/",
                        "httpOnly": false,
                        "secure": false,
                    },
                ]
            }),
        )
        .await;

        let cookies = call.await.unwrap().unwrap();
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].name, "sid");
        assert_eq!(cookies[0].value, "abc");
        assert_eq!(cookies[0].domain, ".example.com");
        assert_eq!(cookies[0].path, "/");
        assert!((cookies[0].expires.unwrap() - 1_700_000_000.5).abs() < 1e-6);
        assert!(cookies[0].http_only);
        assert!(cookies[0].secure);
        assert_eq!(cookies[0].same_site, Some(SameSite::Lax));
        assert_eq!(cookies[1].name, "theme");
        assert!(!cookies[1].http_only);
        assert_eq!(cookies[1].expires, None);
        assert_eq!(cookies[1].same_site, None);

        conn.shutdown();
    }

    /// A jar for the default context ([`CookieJar::new`]) sends neither a
    /// `sessionId` nor a `browserContextId`.
    #[tokio::test]
    async fn default_context_jar_names_no_context() {
        let (mut mock, conn) = MockConnection::pair();
        let jar = CookieJar::new(conn.clone());

        let call = tokio::spawn({
            let j = jar.clone();
            async move { j.all().await }
        });

        let id = mock.expect_cmd("Storage.getCookies").await;
        assert!(mock.last_sent().get("sessionId").is_none());
        assert!(mock.last_sent()["params"].get("browserContextId").is_none());
        mock.reply(id, json!({ "cookies": [] })).await;
        call.await.unwrap().unwrap();

        conn.shutdown();
    }

    /// Every command of a [`CookieJar::for_context`] jar goes to the browser
    /// target (no `sessionId`) and names its context: Chrome rejects
    /// `browserContextId` anywhere else.
    #[tokio::test]
    async fn for_context_names_its_context_in_every_command() {
        let (mut mock, conn) = MockConnection::pair();
        let jar = CookieJar::for_context(conn.clone(), "CTX9");
        let check = |sent: &serde_json::Value| {
            assert!(sent.get("sessionId").is_none(), "{sent}");
            assert_eq!(sent["params"]["browserContextId"], "CTX9", "{sent}");
        };

        let set = tokio::spawn({
            let j = jar.clone();
            async move {
                j.set(Cookie {
                    name: "a".into(),
                    value: "1".into(),
                    domain: ".x.com".into(),
                    path: "/".into(),
                    ..Default::default()
                })
                .await
            }
        });
        let id = mock.expect_cmd("Storage.setCookies").await;
        check(mock.last_sent());
        mock.reply(id, json!({})).await;
        set.await.unwrap().unwrap();

        // delete -> Storage.getCookies, then Storage.setCookies
        let del = tokio::spawn({
            let j = jar.clone();
            async move { j.delete("a", None, None).await }
        });
        let id = mock.expect_cmd("Storage.getCookies").await;
        check(mock.last_sent());
        mock.reply(
            id,
            json!({ "cookies": [{ "name": "a", "value": "1", "domain": ".x.com", "path": "/" }] }),
        )
        .await;
        let id = mock.expect_cmd("Storage.setCookies").await;
        check(mock.last_sent());
        mock.reply(id, json!({})).await;
        del.await.unwrap().unwrap();

        let clr = tokio::spawn({
            let j = jar.clone();
            async move { j.clear().await }
        });
        let id = mock.expect_cmd("Storage.clearCookies").await;
        check(mock.last_sent());
        mock.reply(id, json!({})).await;
        clr.await.unwrap().unwrap();

        conn.shutdown();
    }

    /// [`CookieJar::set`] dispatches `Storage.setCookies` with a one-element
    /// `cookies` array, using CDP's camelCase field names. Singular
    /// `Network.setCookie` was removed in newer Chrome builds; the bulk
    /// endpoint is portable and matches zendriver-python's behaviour.
    #[tokio::test]
    async fn set_dispatches_network_set_cookies_with_camel_case_payload() {
        let (mut mock, conn) = MockConnection::pair();
        let jar = CookieJar::new(conn.clone());

        let call = tokio::spawn({
            let j = jar.clone();
            async move {
                j.set(Cookie {
                    name: "sid".into(),
                    value: "xyz".into(),
                    domain: ".example.com".into(),
                    path: "/".into(),
                    expires: None,
                    http_only: true,
                    secure: true,
                    same_site: Some(SameSite::Strict),
                    url: None,
                    ..Default::default()
                })
                .await
            }
        });

        let id = mock.expect_cmd("Storage.setCookies").await;
        let params = &mock.last_sent()["params"];
        let arr = params["cookies"]
            .as_array()
            .expect("setCookies payload must carry a cookies array");
        assert_eq!(arr.len(), 1);
        let c = &arr[0];
        assert_eq!(c["name"], "sid");
        assert_eq!(c["value"], "xyz");
        assert_eq!(c["domain"], ".example.com");
        assert_eq!(c["path"], "/");
        assert_eq!(c["httpOnly"], true);
        assert_eq!(c["secure"], true);
        assert_eq!(c["sameSite"], "Strict");
        // Snake-case names must NOT appear on the wire.
        assert!(c.get("http_only").is_none());
        assert!(c.get("same_site").is_none());

        mock.reply(id, json!({})).await;
        call.await.unwrap().unwrap();

        conn.shutdown();
    }

    /// The `(name, domain, path)` of the cookies that `jar.delete(name,
    /// domain, path)` overwrites, given the cookies `stored`. Panics unless
    /// each overwrite has an `expires` in the past.
    async fn deleted_by(
        stored: &Value,
        name: &'static str,
        domain: Option<&'static str>,
        path: Option<&'static str>,
    ) -> Vec<(String, String, String)> {
        let (mut mock, conn) = MockConnection::pair();
        let jar = CookieJar::new(conn.clone());
        let call = tokio::spawn(async move { jar.delete(name, domain, path).await });
        let id = mock.expect_cmd("Storage.getCookies").await;
        mock.reply(id, json!({ "cookies": stored })).await;
        let mut deleted = Vec::new();
        if let Some((method, id)) = mock
            .recv_cmd_timeout(std::time::Duration::from_millis(200))
            .await
        {
            assert_eq!(method, "Storage.setCookies");
            for c in mock.last_sent()["params"]["cookies"].as_array().unwrap() {
                assert_eq!(c["expires"], EXPIRED, "not expired: {c}");
                deleted.push((
                    c["name"].as_str().unwrap().to_string(),
                    c["domain"].as_str().unwrap().to_string(),
                    c["path"].as_str().unwrap().to_string(),
                ));
            }
            mock.reply(id, json!({})).await;
        }
        call.await.unwrap().unwrap();
        conn.shutdown();
        deleted
    }

    /// [`CookieJar::delete`] expires exactly the stored cookies with that
    /// name, narrowed by domain (ignoring a leading dot) and path, and sends
    /// nothing when none match.
    #[tokio::test]
    async fn delete_expires_only_the_matching_cookies() {
        let stored = json!([
            { "name": "sid", "value": "1", "domain": ".example.com", "path": "/" },
            { "name": "sid", "value": "2", "domain": "example.com", "path": "/api" },
            { "name": "sid", "value": "3", "domain": "other.com", "path": "/api" },
            { "name": "lang", "value": "4", "domain": ".example.com", "path": "/api" },
        ]);
        let triple = |n: &str, d: &str, p: &str| (n.to_string(), d.to_string(), p.to_string());
        assert_eq!(
            deleted_by(&stored, "sid", None, None).await,
            [
                triple("sid", ".example.com", "/"),
                triple("sid", "example.com", "/api"),
                triple("sid", "other.com", "/api"),
            ]
        );
        assert_eq!(
            deleted_by(&stored, "sid", Some("example.com"), None).await,
            [
                triple("sid", ".example.com", "/"),
                triple("sid", "example.com", "/api"),
            ]
        );
        assert_eq!(
            deleted_by(&stored, "sid", Some(".example.com"), Some("/api")).await,
            [triple("sid", "example.com", "/api")]
        );
        assert_eq!(deleted_by(&stored, "nope", None, None).await, []);
    }

    /// A cookie on `domain` with `path`, `Secure` when `secure`.
    fn cookie(domain: &str, path: &str, secure: bool) -> Cookie {
        Cookie {
            name: "c".into(),
            domain: domain.into(),
            path: path.into(),
            secure,
            ..Default::default()
        }
    }

    fn matches(c: &Cookie, url: &str) -> bool {
        c.matches_url(&url::Url::parse(url).unwrap())
    }

    /// RFC 6265's domain rules: a dotted domain covers its subdomains, a
    /// host-only one just that host, and a suffix isn't a subdomain.
    #[test]
    fn matches_url_by_domain() {
        let dotted = cookie(".example.com", "/", false);
        assert!(matches(&dotted, "http://example.com/"));
        assert!(matches(&dotted, "http://a.b.example.com/"));
        assert!(!matches(&dotted, "http://badexample.com/"));
        assert!(!matches(&dotted, "http://example.org/"));
        let host_only = cookie("example.com", "/", false);
        assert!(matches(&host_only, "http://example.com/"));
        assert!(!matches(&host_only, "http://www.example.com/"));
        assert!(matches(
            &cookie("127.0.0.1", "/", false),
            "http://127.0.0.1:8080/"
        ));
    }

    /// RFC 6265's path-match: the path itself or a path below it, where
    /// "below" starts at a `/`.
    #[test]
    fn matches_url_by_path() {
        let docs = cookie("example.com", "/docs", false);
        assert!(matches(&docs, "http://example.com/docs"));
        assert!(matches(&docs, "http://example.com/docs/a"));
        assert!(!matches(&docs, "http://example.com/docsearch"));
        assert!(!matches(&docs, "http://example.com/"));
        let slash = cookie("example.com", "/docs/", false);
        assert!(matches(&slash, "http://example.com/docs/a"));
        assert!(!matches(&slash, "http://example.com/docs"));
    }

    /// A `Secure` cookie goes to secure schemes and loopback hosts only.
    #[test]
    fn matches_url_by_secure() {
        let secure = cookie("example.com", "/", true);
        assert!(matches(&secure, "https://example.com/"));
        assert!(matches(&secure, "wss://example.com/"));
        assert!(!matches(&secure, "http://example.com/"));
        for loopback in ["http://localhost/", "http://127.0.0.2/", "http://[::1]/"] {
            let host = url::Url::parse(loopback).unwrap();
            let c = cookie(host.host_str().unwrap(), "/", true);
            assert!(c.matches_url(&host), "{loopback}");
        }
    }

    /// A [`Cookie`] carrying the CHIPS / priority extension fields must
    /// surface them on the `Storage.setCookies` wire payload with CDP's
    /// camelCase names + enum string forms.
    #[tokio::test]
    async fn set_cookie_with_priority_and_partition_key_on_wire() {
        let (mut mock, conn) = MockConnection::pair();
        let jar = CookieJar::new(conn.clone());

        let call = tokio::spawn({
            let j = jar.clone();
            async move {
                j.set(Cookie {
                    name: "sid".into(),
                    value: "xyz".into(),
                    domain: ".example.com".into(),
                    path: "/".into(),
                    priority: Some(CookiePriority::High),
                    same_party: Some(true),
                    source_scheme: Some(CookieSourceScheme::Secure),
                    source_port: Some(443),
                    partition_key: Some(CookiePartitionKey::new("https://top")),
                    ..Default::default()
                })
                .await
            }
        });

        let id = mock.expect_cmd("Storage.setCookies").await;
        let params = &mock.last_sent()["params"];
        let c = &params["cookies"]
            .as_array()
            .expect("setCookies payload must carry a cookies array")[0];
        assert_eq!(c["priority"], "High");
        assert_eq!(c["sameParty"], true);
        assert_eq!(c["sourceScheme"], "Secure");
        assert_eq!(c["sourcePort"], 443);
        assert_eq!(c["partitionKey"]["topLevelSite"], "https://top");
        assert_eq!(c["partitionKey"]["hasCrossSiteAncestor"], false);
        // Snake-case names must NOT leak onto the wire.
        assert!(c.get("same_party").is_none());
        assert!(c.get("source_scheme").is_none());
        assert!(c.get("partition_key").is_none());

        mock.reply(id, json!({})).await;
        call.await.unwrap().unwrap();

        conn.shutdown();
    }

    /// Serializing a [`Cookie`] with the new fields to JSON and back must
    /// preserve every value losslessly (snake_case on disk).
    #[test]
    fn cookie_json_roundtrip_preserves_new_fields() {
        let cookie = Cookie {
            name: "sid".into(),
            value: "xyz".into(),
            domain: ".example.com".into(),
            path: "/".into(),
            priority: Some(CookiePriority::Low),
            same_party: Some(false),
            source_scheme: Some(CookieSourceScheme::NonSecure),
            source_port: Some(80),
            partition_key: Some(CookiePartitionKey::new("https://top.example")),
            ..Default::default()
        };

        let json = serde_json::to_string(&cookie).unwrap();
        // snake_case on disk for the public shape.
        assert!(json.contains("\"source_scheme\""));
        assert!(json.contains("\"partition_key\""));

        let back: Cookie = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cookie);
        assert_eq!(back.priority, Some(CookiePriority::Low));
        assert_eq!(back.same_party, Some(false));
        assert_eq!(back.source_scheme, Some(CookieSourceScheme::NonSecure));
        assert_eq!(back.source_port, Some(80));
        assert_eq!(
            back.partition_key,
            Some(CookiePartitionKey::new("https://top.example"))
        );
    }

    /// Deserializing a CDP cookie that omits the new fields must leave them
    /// `None` (serde `default`) and must not panic — this is the
    /// crash-immunity contract that protected rs from the Chrome-146
    /// `sameParty` regression.
    #[test]
    fn cookie_read_missing_new_fields_is_none() {
        let cdp: CdpCookie = serde_json::from_value(json!({
            "name": "sid",
            "value": "xyz",
            "domain": ".example.com",
            "path": "/",
            "httpOnly": true,
            "secure": true,
        }))
        .expect("a CDP cookie lacking the new fields must still parse");
        let cookie: Cookie = cdp.into();

        assert_eq!(cookie.priority, None);
        assert_eq!(cookie.same_party, None);
        assert_eq!(cookie.source_scheme, None);
        assert_eq!(cookie.source_port, None);
        assert_eq!(cookie.partition_key, None);
    }

    /// A CDP cookie carrying the M119+ structured `partitionKey` object must
    /// deserialize into `Some(CookiePartitionKey)` with both fields intact.
    #[test]
    fn cookie_read_structured_partition_key_object() {
        let cdp: CdpCookie = serde_json::from_value(json!({
            "name": "sid",
            "value": "xyz",
            "domain": ".example.com",
            "path": "/",
            "partitionKey": {
                "topLevelSite": "https://top.example",
                "hasCrossSiteAncestor": true,
            },
        }))
        .expect("a CDP cookie with a structured partitionKey must parse");
        let cookie: Cookie = cdp.into();

        assert_eq!(
            cookie.partition_key,
            Some(CookiePartitionKey {
                top_level_site: "https://top.example".into(),
                has_cross_site_ancestor: true,
            })
        );
    }

    /// A future Chrome version may add new CDP cookie fields rs doesn't model
    /// yet. Deserialization must succeed (unknown fields ignored, no panic) —
    /// this is the forward-compat contract guarding against future regressions.
    #[test]
    fn cookie_read_ignores_unknown_future_field() {
        let cdp: CdpCookie = serde_json::from_value(json!({
            "name": "sid", "value": "xyz", "domain": ".example.com", "path": "/",
            "someChrome147Field": { "nested": true }, "anotherNewThing": 42
        }))
        .expect("unknown fields must be ignored, not rejected");
        assert_eq!(cdp.name, "sid");
    }
}
