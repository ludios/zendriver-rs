//! `IpApiResolver`: derive the exit-IP country (and exact timezone, when
//! available) via a proxied HTTP probe.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use tracing::Instrument;
use tracing::instrument::WithSubscriber;
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
/// by every `IpApiResolver` in this process with the same configuration:
/// the same endpoint, proxy (or lack of one), proxy credentials, timeout,
/// and TTL. A resolve that starts while a probe of the same configuration
/// is in flight waits for that probe instead of sending its own.
pub struct IpApiResolver {
    probe: Probe,
    cache_ttl: Duration,
}

/// What one probe asks, and how: the endpoint, the proxy to ask it through
/// (`None`: directly) with that proxy's credentials, and the request
/// timeout.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Probe {
    endpoint: String,
    proxy: Option<String>,
    proxy_auth: Option<(String, String)>,
    timeout: Duration,
}

impl Probe {
    /// Ask the endpoint, through the proxy if one is set, for the exit IP's
    /// country and timezone. `None` means the probe failed; each failure is
    /// logged with its reason.
    async fn run(&self) -> Option<ResolvedGeo> {
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

/// A finished probe's result (`None`: the probe failed), and when it
/// finished.
type Outcome = (Option<ResolvedGeo>, Instant);

/// The [`Outcome`] of one probe, for every resolve that awaits it; pending
/// while the probe is in flight.
type SharedOutcome = Shared<BoxFuture<'static, Outcome>>;

/// The latest probe of each configuration (a [`Probe`] and a cache TTL),
/// shared by every [`IpApiResolver`] in this process. Each resolve first
/// evicts every probe that [`reusable`] rejects for its own TTL.
static PROBES: LazyLock<Mutex<HashMap<(Probe, Duration), SharedOutcome>>> =
    LazyLock::new(Default::default);

/// Whether a resolve whose cache TTL is `ttl` may await `outcome` instead of
/// starting a probe: the probe is in flight, or it succeeded less than `ttl`
/// ago.
fn reusable(outcome: &SharedOutcome, ttl: Duration) -> bool {
    match outcome.peek() {
        None => true,
        Some((geo, finished_at)) => geo.is_some() && finished_at.elapsed() < ttl,
    }
}

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
            probe: Probe {
                endpoint: "http://ip-api.com/json".into(),
                proxy: None,
                proxy_auth: None,
                timeout: Duration::from_secs(5),
            },
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
        self.probe.endpoint = url.into();
        self
    }

    /// Override the request timeout (default 5s).
    #[must_use]
    pub fn timeout(mut self, d: Duration) -> Self {
        self.probe.timeout = d;
        self
    }

    /// Override how long a successful probe is reused (default 1 minute);
    /// `Duration::ZERO` turns off both the reuse and the sharing of a probe
    /// in flight. Reuse and sharing reduce the requests that a burst of
    /// launches sends to the endpoint; `ip-api.com`'s free endpoint allows
    /// 45 requests a minute per IP.
    ///
    /// A successful probe is reused by every later
    /// [`resolve`](GeoResolver::resolve) in this process, of this resolver
    /// or of any other `IpApiResolver` with the same configuration (the
    /// same endpoint, proxy or lack of one, proxy credentials, timeout, and
    /// TTL), until `ttl` has passed since the probe finished. A resolve that
    /// starts while a probe of the same configuration is in flight waits for
    /// that probe and gets its result, even a failure. A resolve that starts
    /// after a probe failed sends a new probe. So a configuration with no
    /// success in the last `ttl` probes again: for example, a new proxy, or
    /// a new session ID in the username of a sticky-session proxy. A probe
    /// runs to its end even when every resolve that awaits it is cancelled.
    ///
    /// The exit IP can change while the configuration stays the same, and a
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
    /// Called by `BrowserBuilder::geo_auto`, wiring the proxy from
    /// [`crate::browser::BrowserBuilder::proxy`] through here. Public so
    /// callers building a custom-endpoint resolver by hand (e.g.
    /// `zendriver-mcp`'s `geo_endpoint` option) can mirror the same proxy
    /// the browser itself uses, the same way `geo_auto`'s bundled default
    /// does.
    #[must_use]
    pub fn with_proxy(mut self, server: Option<String>, auth: Option<(String, String)>) -> Self {
        self.probe.proxy = server;
        self.probe.proxy_auth = auth;
        self
    }

    /// The outcome to await for this resolver's configuration: that of the
    /// probe in flight, or of a success younger than the TTL. Failing both,
    /// that of a new probe, which a task of its own drives to its end, so
    /// that no resolve awaiting it can cancel or stall it, and so that it
    /// finishes even when nothing awaits it any more.
    fn shared_outcome(&self) -> SharedOutcome {
        debug_assert!(!self.cache_ttl.is_zero(), "a zero TTL turns sharing off");
        let key = (self.probe.clone(), self.cache_ttl);
        let mut probes = PROBES.lock().unwrap_or_else(PoisonError::into_inner);
        probes.retain(|(_, ttl), outcome| reusable(outcome, *ttl));
        if let Some(outcome) = probes.get(&key) {
            let outcome = outcome.clone();
            drop(probes);
            tracing::debug!(
                in_flight = outcome.peek().is_none(),
                "geo probe: using a shared probe instead of sending one"
            );
            return outcome;
        }
        let probe = self.probe.clone();
        let outcome = async move { (probe.run().await, Instant::now()) }
            .in_current_span()
            .with_current_subscriber()
            .boxed()
            .shared();
        tokio::spawn(outcome.clone());
        probes.insert(key, outcome.clone());
        outcome
    }
}

#[async_trait]
impl GeoResolver for IpApiResolver {
    /// The result of the probe in flight for this resolver's configuration
    /// (see [`IpApiResolver`]), failed or not; else a success younger than
    /// [`IpApiResolver::cache_ttl`]; else a new probe's result.
    async fn resolve(&self) -> Option<ResolvedGeo> {
        if self.cache_ttl.is_zero() {
            return self.probe.run().await;
        }
        // The probe's own timeout ends it in time, unless its task is on a
        // runtime that nothing drives any more; this bounds the wait anyway,
        // a second later, so that the probe's own warning comes first.
        let bound = self.probe.timeout + Duration::from_secs(1);
        let Ok((geo, _)) = tokio::time::timeout(bound, self.shared_outcome()).await else {
            tracing::warn!("geo probe: the shared probe did not finish in time");
            return None;
        };
        geo
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

    /// A finished probe can be awaited instead of probing only while it is
    /// a success younger than the TTL; a probe in flight always can.
    #[tokio::test]
    async fn reusable_accepts_only_a_probe_in_flight_or_a_recent_success() {
        let minute = Duration::from_secs(60);
        let finished = |geo: Option<ResolvedGeo>| {
            let finished_at = Instant::now()
                .checked_sub(minute)
                .expect("the machine has been up for over a minute");
            let outcome: SharedOutcome =
                futures::future::ready((geo, finished_at)).boxed().shared();
            assert!(outcome.clone().now_or_never().is_some());
            outcome
        };
        let success = finished(Some(ResolvedGeo {
            country: country("DE"),
            timezone: None,
        }));
        assert!(reusable(&success, minute * 2));
        assert!(!reusable(&success, minute));
        assert!(!reusable(&finished(None), minute * 2));

        let in_flight: SharedOutcome = futures::future::pending().boxed().shared();
        assert!(reusable(&in_flight, minute));
    }

    /// Resolvers with one configuration share a success; a resolver whose
    /// configuration differs, if only in its timeout or its TTL, does not,
    /// and neither does one with a zero TTL.
    ///
    /// Each cache test's endpoint is unique to it (here, by its path),
    /// because wiremock's pooled servers repeat ports across tests.
    #[tokio::test]
    async fn only_resolvers_with_one_configuration_share_a_success() {
        let server = answering_de().await;
        let endpoint = format!("{}/cache-configuration", server.uri());
        let resolver = || IpApiResolver::new().endpoint(endpoint.clone());
        let resolvers = [
            (resolver(), 1),
            (resolver(), 1),
            (resolver().timeout(Duration::from_secs(6)), 2),
            (resolver().cache_ttl(Duration::from_secs(61)), 3),
            (resolver().cache_ttl(Duration::ZERO), 4),
        ];
        for (r, requests_after) in resolvers {
            assert_eq!(r.resolve().await.map(|g| g.country), Some(country("DE")));
            assert_eq!(requests(&server).await, requests_after);
        }
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

    /// Concurrent resolves of separate resolvers with one configuration send
    /// one request between them.
    #[tokio::test]
    async fn concurrent_resolves_share_one_probe() {
        let server = answering_de().await;
        let endpoint = format!("{}/cache-concurrent", server.uri());
        let resolvers: Vec<_> = (0..5)
            .map(|_| IpApiResolver::new().endpoint(endpoint.clone()))
            .collect();
        let geos = futures::future::join_all(resolvers.iter().map(|r| r.resolve())).await;
        let de = ResolvedGeo {
            country: country("DE"),
            timezone: None,
        };
        assert_eq!(geos, vec![Some(de); 5]);
        assert_eq!(requests(&server).await, 1);
    }

    /// Wait until `done` holds, polling every 10 ms, for at most 10 s.
    async fn wait_until(what: &str, mut done: impl AsyncFnMut() -> bool) {
        let waited = tokio::time::timeout(Duration::from_secs(10), async {
            while !done().await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(waited.is_ok(), "timed out waiting until {what}");
    }

    /// A probe outlives the resolve that started it: cancelling that resolve
    /// while its request is in flight leaves the probe running, and a later
    /// resolve gets the probe's result instead of sending a request of its
    /// own.
    #[tokio::test]
    async fn cancelling_the_resolve_that_started_a_probe_leaves_the_probe_running() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string(r#"{"countryCode":"DE"}"#)
                    .set_delay(Duration::from_millis(500)),
            )
            .mount(&server)
            .await;
        let endpoint = format!("{}/cache-cancel", server.uri());
        let resolver = || IpApiResolver::new().endpoint(endpoint.clone());

        let first = resolver();
        let mut first = first.resolve();
        assert!(futures::poll!(&mut first).is_pending());
        wait_until("the request is in flight", async || {
            requests(&server).await == 1
        })
        .await;
        drop(first);

        assert_eq!(
            resolver().resolve().await.map(|g| g.country),
            Some(country("DE"))
        );
        assert_eq!(requests(&server).await, 1);
    }

    /// A probe that nothing awaits any more still finishes, so a failure it
    /// ends in is not handed to a later resolve as if it were still in
    /// flight: that resolve probes again.
    #[tokio::test]
    async fn a_probe_finishes_after_every_resolve_awaiting_it_was_cancelled() {
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
        let resolver = IpApiResolver::new().endpoint(format!("{}/cache-abandoned", server.uri()));

        let mut first = resolver.resolve();
        assert!(futures::poll!(&mut first).is_pending());
        drop(first);
        // Another test's resolve may evict the finished probe, which is as
        // good: eviction never touches a probe in flight.
        let key = (resolver.probe.clone(), resolver.cache_ttl);
        let in_flight = || {
            let probes = PROBES.lock().unwrap_or_else(PoisonError::into_inner);
            probes
                .get(&key)
                .is_some_and(|outcome| outcome.peek().is_none())
        };
        wait_until("the abandoned probe has finished", async || !in_flight()).await;

        assert_eq!(
            resolver.resolve().await.map(|g| g.country),
            Some(country("DE"))
        );
        assert_eq!(requests(&server).await, 2);
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
        let endpoint = format!("{}/cache-failure", server.uri());
        let resolver = || IpApiResolver::new().endpoint(endpoint.clone());
        let first = resolver();
        let second = resolver();
        assert_eq!(
            tokio::join!(first.resolve(), second.resolve()),
            (None, None)
        );
        assert_eq!(
            requests(&server).await,
            1,
            "the second resolve must wait for the first"
        );
        assert_eq!(
            resolver().resolve().await.map(|g| g.country),
            Some(country("DE"))
        );
    }
}
