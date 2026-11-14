//! `IpApiResolver`: derive the exit-IP country (and exact timezone, when
//! available) via a proxied HTTP probe.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::OnceCell;
use zendriver_stealth::geo::{Country, GeoResolver, ResolvedGeo};

/// Resolves the apparent country — and, when present, the exact IANA
/// timezone — by querying an IP-geolocation service (default
/// `http://ip-api.com/json` — **plaintext**; the proxy operator can tamper
/// with the response in transit, so override [`Self::endpoint`] to an HTTPS
/// service if response integrity matters for your threat model) through the
/// browser's proxy. Opt-in via `BrowserBuilder::geo_auto`; endpoint
/// overridable; swap the whole thing out with a custom [`GeoResolver`].
///
/// A successful probe is reused for [`Self::cache_ttl`] (default 1 minute)
/// by every `IpApiResolver` in this process that has the same endpoint,
/// proxy, and proxy credentials, and concurrent resolves with those wait
/// for one probe instead of each sending their own.
pub struct IpApiResolver {
    endpoint: String,
    proxy: Option<String>,
    proxy_auth: Option<(String, String)>,
    timeout: Duration,
    cache_ttl: Duration,
}

/// What one cached probe result answers for: one endpoint, asked through one
/// proxy (`None`: directly) with one set of proxy credentials.
#[derive(PartialEq, Eq, Hash)]
struct CacheKey {
    endpoint: String,
    proxy: Option<String>,
    proxy_auth: Option<(String, String)>,
}

/// One probe, shared by every resolve of its [`CacheKey`] that joins it: in
/// flight while `outcome` is empty, then finished.
struct SharedProbe {
    /// The probe's result (`None`: it failed), and when it finished.
    outcome: OnceCell<(Option<ResolvedGeo>, Instant)>,
    /// The TTL of the resolver that started the probe; it bounds how long
    /// the map keeps a finished probe.
    ttl: Duration,
}

impl SharedProbe {
    /// Whether the map must keep this probe: it is in flight, or it
    /// succeeded less than its `ttl` ago.
    ///
    /// A probe whose only holder is the map is not in flight even though
    /// `outcome` is empty: every resolve that started it or waits for it
    /// was cancelled.
    fn is_live(self: &Arc<Self>) -> bool {
        match self.outcome.get() {
            None => Arc::strong_count(self) > 1,
            Some((geo, finished_at)) => geo.is_some() && finished_at.elapsed() < self.ttl,
        }
    }
}

/// The latest probe of each [`CacheKey`], shared by every [`IpApiResolver`]
/// in this process. Each insert first evicts the probes that are no longer
/// live, so the map holds only probes in flight and recent successes.
static PROBES: LazyLock<Mutex<HashMap<CacheKey, Arc<SharedProbe>>>> =
    LazyLock::new(Default::default);

impl Default for IpApiResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl IpApiResolver {
    /// A resolver hitting `http://ip-api.com/json` directly (no proxy), with
    /// a 5-second timeout and a 1-minute [`Self::cache_ttl`]. Chain
    /// [`Self::endpoint`] / [`Self::timeout`] / [`Self::cache_ttl`] to
    /// customize; `BrowserBuilder::geo_auto` wires the proxy (and its
    /// credentials, if any) via [`Self::with_proxy`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            endpoint: "http://ip-api.com/json".into(),
            proxy: None,
            proxy_auth: None,
            timeout: Duration::from_secs(5),
            cache_ttl: Duration::from_secs(60),
        }
    }

    /// Override the probe endpoint (default `http://ip-api.com/json`, which
    /// is plaintext HTTP — a malicious or compromised proxy operator can
    /// observe or tamper with the response since it's routed through
    /// `with_proxy`; override to an HTTPS endpoint if you need integrity).
    /// Must return a JSON body with a top-level `countryCode` string field;
    /// an optional top-level `timezone` string field (ip-api's exact IANA
    /// zone for the exit IP) is used when present, falling back to the
    /// country-representative zone otherwise.
    #[must_use]
    pub fn endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = url.into();
        self
    }

    /// Override the request timeout (default 5s).
    #[must_use]
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Override how long a successful probe is reused (default 1 minute);
    /// `Duration::ZERO` turns the reuse off, and with it the sharing of a
    /// probe in flight. The reuse keeps a burst of launches under the rate
    /// limit of `ip-api.com`'s free endpoint (45 requests a minute per IP).
    ///
    /// A probe result is reused by every later [`resolve`](GeoResolver::resolve)
    /// in this process, of this resolver or of any other `IpApiResolver`,
    /// that has the same endpoint, the same proxy (or no proxy), and the
    /// same proxy credentials, until `ttl` has passed since the probe
    /// finished. So changing the proxy, or the session ID that a
    /// sticky-session proxy takes in its username, makes the next launch
    /// probe again. Resolves like that which start while the probe is in
    /// flight wait for it and get its result, even a failure; a resolve
    /// that starts after a probe failed probes again.
    ///
    /// The exit IP can change while all of those stay the same, and a
    /// reused result then belongs to the earlier exit IP until `ttl` has
    /// passed: without a proxy, a VPN or a switch of networks changes this
    /// machine's own exit IP, and a rotating proxy can change its exit IP
    /// between any two connections.
    #[must_use]
    pub fn cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// Route the probe through `server` (mirrors the browser's own proxy so
    /// the resolved country matches the exit IP Chrome will actually use),
    /// authenticating with `auth` (`user`, `pass`) when the proxy requires
    /// it — passed to reqwest via [`reqwest::Proxy::basic_auth`], never
    /// embedded in the proxy URL string, so it can't leak into an error
    /// `Display`.
    ///
    /// Called by `BrowserBuilder::geo_auto`, wiring `self.proxy` from
    /// [`crate::browser::BrowserBuilder::proxy`] through here. Public so
    /// callers building a custom-endpoint resolver by hand (e.g.
    /// `zendriver-mcp`'s `geo_endpoint` option) can mirror the same proxy
    /// the browser itself uses, the same way `geo_auto`'s bundled default
    /// does.
    #[must_use]
    pub fn with_proxy(mut self, server: Option<String>, auth: Option<(String, String)>) -> Self {
        self.proxy = server;
        self.proxy_auth = auth;
        self
    }

    /// The key under which this resolver's results are cached, or `None`
    /// when the TTL is zero, which turns caching off.
    fn cache_key(&self) -> Option<CacheKey> {
        if self.cache_ttl.is_zero() {
            return None;
        }
        Some(CacheKey {
            endpoint: self.endpoint.clone(),
            proxy: self.proxy.clone(),
            proxy_auth: self.proxy_auth.clone(),
        })
    }

    /// The probe of `key` to wait for: the one in flight, or a success less
    /// than this resolver's TTL old. Failing both, a new, empty probe, which
    /// replaces the old one; the first resolve to wait for it runs it.
    fn shared_probe(&self, key: CacheKey) -> Arc<SharedProbe> {
        let mut probes = PROBES.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(probe) = probes.get(&key) {
            match probe.outcome.get() {
                None => {
                    tracing::debug!(
                        endpoint = %self.endpoint,
                        "geo probe: waiting for the one in flight"
                    );
                    return probe.clone();
                }
                Some((Some(_), finished_at)) if finished_at.elapsed() < self.cache_ttl => {
                    tracing::debug!(
                        endpoint = %self.endpoint,
                        "geo probe: reusing a recent result"
                    );
                    return probe.clone();
                }
                Some(_) => {}
            }
        }
        probes.retain(|_, probe| probe.is_live());
        let probe = Arc::new(SharedProbe {
            outcome: OnceCell::new(),
            ttl: self.cache_ttl,
        });
        probes.insert(key, probe.clone());
        probe
    }

    /// Ask the endpoint, through the proxy if one is set, for the exit IP's
    /// country and timezone. `None` means the probe failed; each failure is
    /// logged with its reason.
    async fn probe(&self) -> Option<ResolvedGeo> {
        // reqwest 0.13 installs no rustls crypto provider of its own (see the workspace manifest),
        // and the omission surfaces as a runtime panic rather than a build error. Install before
        // the client exists.
        crate::tls::install_default_crypto_provider();
        let mut builder = reqwest::Client::builder().timeout(self.timeout);
        if let Some(p) = &self.proxy {
            match reqwest::Proxy::all(p) {
                Ok(mut px) => {
                    if let Some((user, pass)) = &self.proxy_auth {
                        px = px.basic_auth(user, pass);
                    }
                    builder = builder.proxy(px);
                }
                Err(e) => {
                    // `reqwest::Error`'s `Display` only ever includes the
                    // failed proxy/target URL text, never proxy credentials
                    // (those are sent via `basic_auth`, not embedded in the
                    // URL) — safe to log as-is.
                    tracing::warn!(error = %e, "geo probe: bad proxy; skipping");
                    return None;
                }
            }
        }
        // Every `None` below is a *failure*, not "this IP has no country" —
        // the caller downgrades the persona either way, so each exit says
        // which one it was.
        let client = match builder.build() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "geo probe: HTTP client build failed; skipping");
                return None;
            }
        };
        let resp = match client.get(&self.endpoint).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "geo probe request failed");
                return None;
            }
        };
        let status = resp.status();
        let body: serde_json::Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    status = %status,
                    endpoint = %self.endpoint,
                    "geo probe: response body was not JSON"
                );
                return None;
            }
        };
        let Some(cc) = body.get("countryCode").and_then(|v| v.as_str()) else {
            tracing::warn!(
                status = %status,
                endpoint = %self.endpoint,
                "geo probe: response has no string `countryCode` field"
            );
            return None;
        };
        let country = match Country::try_from(cc) {
            Ok(c) => c,
            Err(_) => {
                tracing::warn!(country = %cc, "geo probe: unrecognized country code");
                return None;
            }
        };
        let timezone = body
            .get("timezone")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Some(ResolvedGeo { country, timezone })
    }
}

#[async_trait]
impl GeoResolver for IpApiResolver {
    /// The result of the probe in flight for this endpoint, proxy, and proxy
    /// credentials, failed or not; else a success younger than
    /// [`IpApiResolver::cache_ttl`]; else a new probe's result.
    async fn resolve(&self) -> Option<ResolvedGeo> {
        let Some(key) = self.cache_key() else {
            return self.probe().await;
        };
        let shared = self.shared_probe(key);
        let (geo, _) = shared
            .outcome
            .get_or_init(|| async { (self.probe().await, Instant::now()) })
            .await;
        geo.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_capture::{CapturedLog, sole_warning, warnings, with_captured_logs};

    /// Stand a mock endpoint up answering `status` with `body`, run an
    /// uncached [`IpApiResolver`] against it, and return the resolution plus
    /// everything logged while it ran.
    ///
    /// Every probe test needs the same four lines of wiremock scaffolding;
    /// this is them, once. The cache is off because wiremock hands its
    /// pooled servers, ports included, from test to test, so the same
    /// endpoint URL would return another test's cached result.
    async fn probe(status: u16, body: &str) -> (Option<ResolvedGeo>, Vec<CapturedLog>) {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        let resolver = IpApiResolver::new()
            .endpoint(server.uri())
            .cache_ttl(Duration::ZERO);
        with_captured_logs(async move { resolver.resolve().await }).await
    }

    /// A mock server answering every GET with `{"countryCode":"DE"}`.
    async fn answering_de() -> wiremock::MockServer {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_string(r#"{"countryCode":"DE"}"#),
            )
            .mount(&server)
            .await;
        server
    }

    /// How many requests `server` has received.
    async fn requests(server: &wiremock::MockServer) -> usize {
        server
            .received_requests()
            .await
            .expect("wiremock records requests by default")
            .len()
    }

    fn country(cc: &str) -> Country {
        Country::try_from(cc).expect("test country code must be valid")
    }

    #[tokio::test]
    async fn resolves_country_from_ipapi_json() {
        let (geo, logs) = probe(200, r#"{"countryCode":"DE"}"#).await;
        assert_eq!(
            geo,
            Some(ResolvedGeo {
                country: country("DE"),
                timezone: None,
            })
        );
        assert!(
            warnings(&logs).is_empty(),
            "a probe that succeeded must warn about nothing: {:?}",
            warnings(&logs),
        );
    }

    /// A body carrying ip-api's `timezone` field must thread it through as
    /// the EXACT probe timezone, not just the country.
    #[tokio::test]
    async fn resolves_exact_timezone_from_ipapi_json() {
        let (geo, _) = probe(
            200,
            r#"{"countryCode":"US","timezone":"America/Los_Angeles"}"#,
        )
        .await;
        assert_eq!(
            geo,
            Some(ResolvedGeo {
                country: country("US"),
                timezone: Some("America/Los_Angeles".to_string()),
            })
        );
    }

    /// A body with `countryCode` but no `timezone` field must yield
    /// `timezone: None` (falls back to the country-representative zone
    /// downstream), not an error.
    #[tokio::test]
    async fn missing_timezone_field_yields_none_timezone() {
        let (geo, logs) = probe(200, r#"{"countryCode":"US"}"#).await;
        let resolved = geo.expect("a country with no timezone is still a resolution");
        assert_eq!(resolved.country, country("US"));
        assert_eq!(resolved.timezone, None);
        assert!(
            warnings(&logs).is_empty(),
            "a missing timezone is not a failure: {:?}",
            warnings(&logs),
        );
    }

    /// Well-formed JSON that simply has no `countryCode` (ip-api answers
    /// `{"status":"fail",...}` for a reserved-range IP) is a probe FAILURE,
    /// not geo data.
    ///
    /// It yielded `None` before this change too — the change is that it now
    /// says so instead of downgrading the persona in silence, so the
    /// warning is the only thing here worth asserting.
    #[tokio::test]
    async fn missing_country_code_warns_and_yields_none() {
        let (geo, logs) = probe(200, r#"{"status":"fail","message":"reserved range"}"#).await;
        assert_eq!(geo, None);
        let warning = sole_warning(&logs);
        assert!(
            warning.contains("no string `countryCode` field"),
            "unexpected warning: {warning}",
        );
    }

    /// A body that is not JSON at all is one failure with one cause,
    /// whether it arrives as a 200 or as a proxy's 502 HTML error page:
    /// same `resp.json()` code path, same warning. Both cases live here
    /// rather than in two tests that differ only in bytes neither asserts
    /// on.
    #[tokio::test]
    async fn a_non_json_body_warns_and_yields_none() {
        for (status, body) in [
            (200, "nope"),
            (502, "<html><body>Bad Gateway</body></html>"),
        ] {
            let (geo, logs) = probe(status, body).await;
            assert_eq!(geo, None, "status {status}");
            let warning = sole_warning(&logs);
            assert!(
                warning.contains("response body was not JSON"),
                "status {status}: unexpected warning: {warning}",
            );
        }
    }

    /// A `countryCode` that is not a two-letter code is a failure too, and
    /// a distinct one — the body parsed, the field was there, the value was
    /// junk. It must not be reported as a missing field.
    #[tokio::test]
    async fn an_unparseable_country_code_warns_and_yields_none() {
        let (geo, logs) = probe(200, r#"{"countryCode":"XYZ"}"#).await;
        assert_eq!(geo, None);
        let warning = sole_warning(&logs);
        assert!(
            warning.contains("unrecognized country code"),
            "unexpected warning: {warning}",
        );
    }

    /// C1: `with_proxy`'s `auth` must actually reach the underlying
    /// `reqwest::Proxy` (via `basic_auth`), not just be stored and ignored.
    /// A real authenticated-proxy wiremock (mocking the CONNECT/tunnel
    /// handshake a real forward proxy performs) is impractical with
    /// `wiremock` (it's an HTTP server, not a proxy), so this instead stands
    /// a plain wiremock server in for the proxy and drives a plain-HTTP
    /// target through it — reqwest relays plain-HTTP-through-HTTP-proxy
    /// requests as absolute-form requests directly to the proxy's socket
    /// with a `Proxy-Authorization` header when `basic_auth` was set, so the
    /// mock server IS the thing that receives (and can assert on) that
    /// header. If `with_proxy`'s `auth` were dropped (the C1 bug), the mock
    /// (which requires the header) would never match and `resolve()` would
    /// return `None`.
    #[tokio::test]
    async fn threads_proxy_credentials_into_reqwest_proxy() {
        let proxy = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::header_exists("Proxy-Authorization"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_string(r#"{"countryCode":"DE"}"#),
            )
            .mount(&proxy)
            .await;

        let r = IpApiResolver::new()
            .endpoint("http://geo-probe.invalid/json")
            .with_proxy(Some(proxy.uri()), Some(("bob".into(), "s3cret".into())));
        assert_eq!(r.resolve().await.map(|g| g.country), Some(country("DE")));
    }

    /// Without credentials, the mock (which requires `Proxy-Authorization`)
    /// must NOT match — a control proving the above test isn't a false
    /// positive from some other matcher laxity.
    #[tokio::test]
    async fn no_credentials_means_no_proxy_auth_header() {
        let proxy = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::header_exists("Proxy-Authorization"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_string(r#"{"countryCode":"DE"}"#),
            )
            .mount(&proxy)
            .await;

        let r = IpApiResolver::new()
            .endpoint("http://geo-probe.invalid/json")
            .with_proxy(Some(proxy.uri()), None);
        // No `Proxy-Authorization` header sent -> mock doesn't match -> 404
        // from wiremock -> `.json()` fails -> `resolve()` yields `None`.
        assert_eq!(r.resolve().await, None);
    }

    /// A proxy URL reqwest cannot parse is a failure before any request is
    /// made, and the warning must not carry the credentials that would have
    /// gone with it.
    #[tokio::test]
    async fn a_bad_proxy_url_warns_without_leaking_credentials() {
        let resolver = IpApiResolver::new()
            .endpoint("http://geo-probe.invalid/json")
            .with_proxy(
                Some("not a url".into()),
                Some(("bob".into(), "s3cret".into())),
            );
        let (geo, logs) = with_captured_logs(async move { resolver.resolve().await }).await;
        assert_eq!(geo, None);
        let warning = sole_warning(&logs);
        assert!(
            warning.contains("bad proxy"),
            "unexpected warning: {warning}"
        );
        assert!(
            !warning.contains("s3cret") && !warning.contains("bob"),
            "proxy credentials must never reach the log: {warning}",
        );
    }

    /// A second resolver with the same endpoint and no proxy reuses the
    /// first one's probe. A resolver with a zero TTL probes anyway, and so
    /// does one whose TTL the cached result has outlived.
    ///
    /// Each cache test's endpoint is unique to it (here, by its path),
    /// because wiremock's pooled servers repeat ports across tests.
    #[tokio::test]
    async fn a_fresh_result_is_reused_and_an_expired_one_is_not() {
        let server = answering_de().await;
        let endpoint = format!("{}/cache-reuse", server.uri());
        let resolver = || IpApiResolver::new().endpoint(endpoint.clone());
        let resolvers = [
            resolver(),
            resolver(),
            resolver().cache_ttl(Duration::ZERO),
            resolver().cache_ttl(Duration::from_nanos(1)),
        ];
        for r in resolvers {
            assert_eq!(r.resolve().await.map(|g| g.country), Some(country("DE")));
        }
        assert_eq!(
            requests(&server).await,
            3,
            "only the second resolver may reuse the first one's probe",
        );
    }

    /// New proxy credentials (e.g. a new sticky-session ID in the username)
    /// probe again, and the first credentials still have their own result.
    #[tokio::test]
    async fn each_set_of_proxy_credentials_gets_its_own_result() {
        let proxy = answering_de().await;
        let resolver = |user: &str| {
            IpApiResolver::new()
                .endpoint("http://geo-cache-credentials.invalid/json")
                .with_proxy(Some(proxy.uri()), Some((user.into(), "pw".into())))
        };
        for user in ["session-1", "session-2", "session-1"] {
            let geo = resolver(user).resolve().await;
            assert_eq!(geo.map(|g| g.country), Some(country("DE")), "{user}");
        }
        assert_eq!(requests(&proxy).await, 2);
    }

    /// Concurrent resolves with one key send one request between them.
    #[tokio::test]
    async fn concurrent_resolves_share_one_probe() {
        let server = answering_de().await;
        let resolver = IpApiResolver::new().endpoint(format!("{}/cache-concurrent", server.uri()));
        let geos = futures::future::join_all((0..5).map(|_| resolver.resolve())).await;
        assert_eq!(
            geos,
            vec![
                Some(ResolvedGeo {
                    country: country("DE"),
                    timezone: None
                });
                5
            ]
        );
        assert_eq!(requests(&server).await, 1);
    }

    /// The resolves waiting for a probe in flight share its failure, but a
    /// resolve that starts after the probe failed probes again instead of
    /// failing again.
    #[tokio::test]
    async fn a_failed_probe_is_shared_in_flight_but_not_reused() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(502).set_body_string("Bad Gateway"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_string(r#"{"countryCode":"DE"}"#),
            )
            .mount(&server)
            .await;
        let resolver = IpApiResolver::new().endpoint(format!("{}/cache-failure", server.uri()));
        let (first, second) = tokio::join!(resolver.resolve(), resolver.resolve());
        assert_eq!((first, second), (None, None));
        assert_eq!(
            requests(&server).await,
            1,
            "the second resolve must wait for the first"
        );
        assert_eq!(
            resolver.resolve().await.map(|g| g.country),
            Some(country("DE"))
        );
    }
}
