//! [`InterceptBuilder`] — fluent rule + pattern registration.
//!
//! Two-phase API:
//! - **Configure**: chain [`block`], [`block_hosts`], [`redirect`], [`respond`],
//!   [`modify_request`], [`modify_response`] for declarative rules, plus
//!   [`pattern`] / [`at_request`] / [`at_response`] / [`resource`] to control
//!   which CDP `Fetch.RequestPattern` entries are sent on `Fetch.enable`.
//! - **Activate**: [`start`](InterceptBuilder::start) adds the rules, with
//!   the patterns, to the session's [`Interceptor`] as one layer, returning
//!   an [`InterceptHandle`] that removes the layer when dropped.
//!   Alternatively, [`subscribe`](InterceptBuilder::subscribe) adds a layer
//!   that hands each paused request to the returned [`InterceptStream`], for
//!   the caller to release.
//!
//! `Tab::intercept()` in `zendriver` builds on the tab's [`Interceptor`].
//!
//! [`block`]: InterceptBuilder::block
//! [`block_hosts`]: InterceptBuilder::block_hosts
//! [`redirect`]: InterceptBuilder::redirect
//! [`respond`]: InterceptBuilder::respond
//! [`modify_request`]: InterceptBuilder::modify_request
//! [`modify_response`]: InterceptBuilder::modify_response
//! [`pattern`]: InterceptBuilder::pattern
//! [`at_request`]: InterceptBuilder::at_request
//! [`at_response`]: InterceptBuilder::at_response
//! [`resource`]: InterceptBuilder::resource

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::error::InterceptionError;
use crate::host_matcher::HostMatcher;
use crate::interceptor::{Filter, InterceptHandle, InterceptStream, Interceptor, Layer, Responder};
use crate::rule::Rule;
use crate::types::{
    RequestInfo, RequestOverrides, RequestStage, ResourceType, ResponseInfo, ResponseOverrides,
};
use crate::url_pattern::UrlPattern;

/// A pending `Fetch.RequestPattern` entry to send on `Fetch.enable`.
///
/// CDP's [`Fetch.RequestPattern`] takes an optional `urlPattern`,
/// `resourceType`, and `requestStage`. We mirror it 1:1 here. The builder
/// accumulates these via [`InterceptBuilder::pattern`] / `at_request` /
/// `at_response` / `resource`, mutating the last-pushed entry per chain — so
/// `builder.pattern("*").at_response().resource(Image)` produces a single
/// `RequestPattern` with all three fields set.
///
/// [`Fetch.RequestPattern`]: https://chromedevtools.github.io/devtools-protocol/tot/Fetch/#type-RequestPattern
#[derive(Debug, Clone, Default)]
pub struct RequestPattern {
    /// URL pattern in CDP wildcard syntax. `None` means "match any URL"
    /// (CDP default).
    pub url_pattern: Option<String>,
    /// Resource type filter (e.g. `Image`, `XHR`). `None` means "all types".
    pub resource_type: Option<ResourceType>,
    /// Lifecycle stage at which to pause. `None` means CDP's default
    /// (`Request`).
    pub request_stage: Option<RequestStage>,
}

/// Fluent builder for one layer of a session's interception.
///
/// Construct via `Tab::intercept()` (gated `feature = "interception"`) or
/// [`InterceptBuilder::new`]. Chain configuration methods to register rules
/// and declare the CDP `Fetch` patterns, then call [`start`](Self::start) to
/// have the rules applied, or [`subscribe`](Self::subscribe) to release each
/// paused request yourself.
//
// `Debug` works because `Rule` has a hand-written `Debug` impl that renders
// the closure variant's body as `<closure>`. Inner `Vec<Rule>` derives via
// that.
#[derive(Debug)]
pub struct InterceptBuilder {
    interceptor: Interceptor,
    patterns: Vec<RequestPattern>,
    rules: Vec<Rule>,
    /// Optional proxy credentials. When set, `Fetch.enable` is sent with
    /// `handleAuthRequests: true` and a proxy's `Fetch.authRequired` is
    /// answered with these credentials. See cdpdriver/zendriver#208.
    proxy_auth: Option<(String, String)>,
    /// The pattern that pauses requests for `proxy_auth`, whose challenges
    /// Chrome surfaces only for paused requests: every request's, or only
    /// documents' ([`handle_proxy_auth_scoped_to_document`]).
    ///
    /// [`handle_proxy_auth_scoped_to_document`]: Self::handle_proxy_auth_scoped_to_document
    auth_pattern: RequestPattern,
}

impl InterceptBuilder {
    /// Construct a fresh builder for a layer of `interceptor`'s session.
    ///
    /// End users go through `Tab::intercept()` rather than calling this
    /// directly.
    ///
    /// ```no_run
    /// # async fn ex(interceptor: &zendriver_interception::Interceptor)
    /// #   -> Result<(), zendriver_interception::InterceptionError> {
    /// use zendriver_interception::InterceptBuilder;
    ///
    /// let _handle = InterceptBuilder::new(interceptor)
    ///     .block("*/tracker.js")?
    ///     .start()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn new(interceptor: &Interceptor) -> Self {
        Self {
            interceptor: interceptor.clone(),
            patterns: Vec::new(),
            rules: Vec::new(),
            proxy_auth: None,
            auth_pattern: RequestPattern {
                url_pattern: Some("*".into()),
                ..RequestPattern::default()
            },
        }
    }

    /// Answer a proxy's auth challenges with the given credentials.
    ///
    /// `Fetch.enable` is sent with `handleAuthRequests: true`, and each
    /// `Fetch.authRequired` whose challenge comes from a proxy is answered
    /// with `Fetch.continueWithAuth { authChallengeResponse: { response:
    /// "ProvideCredentials", username, password } }`. Each request gets the
    /// credentials once per proxy and auth scheme: if the same proxy
    /// challenges it again with the same scheme within a minute, which means
    /// it rejected them, the challenge is cancelled and the request fails.
    /// (After a PAC failover to another proxy, or a fallback to another
    /// scheme, the request gets them again, and so does a repeat that comes
    /// over a minute later.) A
    /// challenge from the server (HTTP 401) gets `Default`, never these
    /// credentials, so a site can't collect them. To log into a site's own
    /// HTTP auth, add an `Authorization` header to its requests with
    /// [`modify_request`](Self::modify_request).
    ///
    /// Composes with rules, and with [`subscribe`](Self::subscribe). While
    /// several layers of a session have credentials, the newest layer's go
    /// to the proxy. `BrowserBuilder::proxy_auth` in the `zendriver` crate
    /// installs this for you.
    ///
    /// See cdpdriver/zendriver#208.
    #[must_use]
    pub fn handle_proxy_auth(mut self, user: impl Into<String>, pass: impl Into<String>) -> Self {
        self.proxy_auth = Some((user.into(), pass.into()));
        self
    }

    /// Answer a proxy's auth challenges with the given credentials, for
    /// `Document`-type requests only.
    ///
    /// Like [`handle_proxy_auth`](Self::handle_proxy_auth), but for the
    /// credentials the layer pauses only document requests (`*` of
    /// `ResourceType::Document`) rather than every request: Chrome surfaces
    /// a proxy's challenge only for a request it paused. The rules' own
    /// patterns are unchanged.
    ///
    /// Narrowing to `Document` means only the navigations of the page and
    /// its frames pause and round-trip through the multiplexer for the
    /// credentials; other requests (XHR, subresources, and any
    /// parallel/pipelined connection that also needs credentials) are not
    /// paused for them, and their auth challenges are not answered unless
    /// another pattern pauses them. Use plain
    /// [`handle_proxy_auth`](Self::handle_proxy_auth) if you need every
    /// connection's 407 covered.
    #[must_use]
    pub fn handle_proxy_auth_scoped_to_document(
        mut self,
        user: impl Into<String>,
        pass: impl Into<String>,
    ) -> Self {
        self.auth_pattern.resource_type = Some(ResourceType::Document);
        self.handle_proxy_auth(user, pass)
    }

    /// Push a new pattern entry with the given URL pattern string.
    ///
    /// Subsequent [`at_request`](Self::at_request) /
    /// [`at_response`](Self::at_response) / [`resource`](Self::resource) calls
    /// mutate this newest entry, so a chain like
    /// `.pattern("*").at_response().resource(ResourceType::XHR)` produces one
    /// `RequestPattern` with all three fields populated.
    #[must_use]
    pub fn pattern(mut self, pattern: impl Into<String>) -> Self {
        self.patterns.push(RequestPattern {
            url_pattern: Some(pattern.into()),
            ..RequestPattern::default()
        });
        self
    }

    /// Pause matching requests at the `Request` stage on the most-recently
    /// pushed pattern.
    ///
    /// If no pattern has been pushed yet, this creates an empty one (matches
    /// every URL by CDP default) and sets the stage on it.
    #[must_use]
    pub fn at_request(mut self) -> Self {
        self.ensure_pattern().request_stage = Some(RequestStage::Request);
        self
    }

    /// Pause matching requests at the `Response` stage on the most-recently
    /// pushed pattern.
    #[must_use]
    pub fn at_response(mut self) -> Self {
        self.ensure_pattern().request_stage = Some(RequestStage::Response);
        self
    }

    /// Restrict the most-recently pushed pattern to a single resource type.
    #[must_use]
    pub fn resource(mut self, kind: ResourceType) -> Self {
        self.ensure_pattern().resource_type = Some(kind);
        self
    }

    /// Register a [`Rule::Block`] for `pattern`.
    ///
    /// Compiles `pattern` eagerly; an invalid pattern fails the builder chain
    /// with [`InterceptionError::InvalidPattern`] returned as `Err(Self)` via
    /// the `Result` wrapper.
    pub fn block(mut self, pattern: impl Into<String>) -> Result<Self, InterceptionError> {
        self.rules.push(Rule::Block {
            pattern: UrlPattern::new(pattern)?,
        });
        Ok(self)
    }

    /// Register a [`Rule::BlockHosts`] backed by `matcher`.
    ///
    /// Every request whose host is in `matcher` (exact, or a parent domain on
    /// a dot boundary) is failed with `BlockedByClient`. Composes with other
    /// rules in registration order. `zendriver` core's tracker-blocklist
    /// wiring uses this; most callers reach it via `BrowserBuilder::block_trackers`.
    #[must_use]
    pub fn block_hosts(mut self, matcher: Arc<HostMatcher>) -> Self {
        self.rules.push(Rule::BlockHosts { matcher });
        self
    }

    /// Register a [`Rule::Redirect`] that rewrites `from` → `to`.
    pub fn redirect(
        mut self,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Result<Self, InterceptionError> {
        self.rules.push(Rule::Redirect {
            from: UrlPattern::new(from)?,
            to: to.into(),
        });
        Ok(self)
    }

    /// Register a [`Rule::Respond`] serving a synthesized response.
    pub fn respond(
        mut self,
        pattern: impl Into<String>,
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<Self, InterceptionError> {
        self.rules.push(Rule::Respond {
            pattern: UrlPattern::new(pattern)?,
            status,
            headers,
            body,
        });
        Ok(self)
    }

    /// Register a [`Rule::Modify`] driven by a user closure.
    ///
    /// The closure runs on the multiplexer's task per matching request — it must be
    /// `Send + Sync` and `'static`. Wrap shared state in `Arc` if needed.
    pub fn modify_request<F>(
        mut self,
        pattern: impl Into<String>,
        modify: F,
    ) -> Result<Self, InterceptionError>
    where
        F: Fn(&RequestInfo) -> RequestOverrides + Send + Sync + 'static,
    {
        self.rules.push(Rule::Modify {
            pattern: UrlPattern::new(pattern)?,
            modify: Arc::new(modify),
        });
        Ok(self)
    }

    /// Register a [`Rule::ModifyResponse`] driven by a user closure.
    ///
    /// The closure rewrites an upstream response's status/headers (keeping
    /// Chrome's body) and only fires at the `Response` stage. Without
    /// explicit [`pattern`](Self::pattern)s, the layer pauses there for it;
    /// with them, give one [`at_response`](Self::at_response).
    /// Header overrides are *replacement*, not merge (CDP semantics): return
    /// every header you want forwarded.
    ///
    /// Like [`modify_request`](Self::modify_request), the closure runs on the
    /// multiplexer's task per matching response, so it must be `Send + Sync` and
    /// `'static`. Wrap shared state in `Arc` if needed.
    pub fn modify_response<F>(
        mut self,
        pattern: impl Into<String>,
        modify: F,
    ) -> Result<Self, InterceptionError>
    where
        F: Fn(&ResponseInfo) -> ResponseOverrides + Send + Sync + 'static,
    {
        self.rules.push(Rule::ModifyResponse {
            pattern: UrlPattern::new(pattern)?,
            modify: Arc::new(modify),
        });
        Ok(self)
    }

    /// Add the rules to the session's interception as one layer, and return
    /// once Chrome pauses the requests they need: requests sent from then
    /// on are intercepted.
    ///
    /// The layer stays in force until the returned [`InterceptHandle`] is
    /// dropped or [stopped](InterceptHandle::stop). Layers added later take
    /// precedence: each paused request goes to the newest layer that has a
    /// pattern and a rule for it.
    ///
    /// If no [`pattern`](Self::pattern) entries were added, the patterns
    /// come from the rules: each rule's URL pattern, at the stage it acts
    /// at (`Response` for [`modify_response`](Self::modify_response),
    /// `Request` for the others), and `"*"` at the `Request` stage for
    /// [`block_hosts`](Self::block_hosts) and for
    /// [`handle_proxy_auth`](Self::handle_proxy_auth). Chrome then pauses
    /// only the requests some rule acts on.
    ///
    /// # Errors
    ///
    /// [`InterceptionError::InvalidPattern`] for a pattern that doesn't
    /// compile, and [`InterceptionError::Call`] if Chrome refused the
    /// patterns, or the session is gone.
    pub async fn start(self) -> Result<InterceptHandle, InterceptionError> {
        let interceptor = self.interceptor.clone();
        let layer = self.into_layer(Responder::Rules)?;
        interceptor.add(layer).await
    }

    /// Add a layer that hands each request it pauses to the returned
    /// [`InterceptStream`], and return once Chrome pauses them.
    ///
    /// The layer uses the declared patterns (a single match-all `"*"`
    /// pattern at the `Request` stage when none were added), and takes
    /// every request they pause, before any older layer sees it. Release each
    /// [`PausedRequest`](crate::PausedRequest) with one of its terminal
    /// methods (`continue_` / `abort` / `respond` / `modify_and_continue` /
    /// `continue_response`): Chrome holds the request until then.
    ///
    /// Rules registered via `block` / `redirect` / `respond` /
    /// `modify_request` are ignored on this path: stream consumers drive
    /// every paused request themselves. Use [`start`](Self::start) when you
    /// want them applied. [`handle_proxy_auth`](Self::handle_proxy_auth)
    /// credentials are honored: a proxy's challenge never reaches the
    /// stream.
    ///
    /// Dropping the stream removes the layer, and continues each request it
    /// took that wasn't read yet.
    ///
    /// # Errors
    ///
    /// As for [`start`](Self::start).
    pub async fn subscribe(self) -> Result<InterceptStream, InterceptionError> {
        let interceptor = self.interceptor.clone();
        let (requests, received) = mpsc::unbounded_channel();
        let layer = self.into_layer(|_| Responder::Stream(requests))?;
        Ok(InterceptStream {
            requests: received,
            _layer: interceptor.add(layer).await?,
        })
    }

    /// The layer this builder describes, its rules taken by `responder`.
    ///
    /// Without explicit patterns, the layer's come from what it does (see
    /// [`start`](Self::start)): each rule's, a match-all one for a stream,
    /// and `auth_pattern` (at the `Request` stage) for proxy credentials.
    /// With explicit patterns, only the Document-scoped `auth_pattern` is
    /// added to them.
    /// Chrome (~M132+) rejects `Fetch.enable { patterns: [],
    /// handleAuthRequests: true }` with CDP error -32602 "Can't specify
    /// empty patterns with handleAuth set", and only surfaces the
    /// challenges of requests that a Request-stage pattern paused.
    ///
    /// On the auth-only path this pauses (and immediately continues) every
    /// request. If that per-request CDP round-trip regresses a
    /// timing-sensitive bot sensor, a caller can register an explicit
    /// narrower pattern (e.g. `resource_type: Document`, so only the main
    /// navigation — which carries the first proxy 407 — is paused), at the
    /// risk of missing 407s on parallel proxy connections.
    fn into_layer(
        mut self,
        responder: impl FnOnce(Vec<Rule>) -> Responder,
    ) -> Result<Layer, InterceptionError> {
        let responder = responder(self.rules);
        let match_all = || RequestPattern {
            url_pattern: Some("*".into()),
            ..RequestPattern::default()
        };
        let documents_only = self.auth_pattern.resource_type.is_some();
        if self.patterns.is_empty() {
            match &responder {
                Responder::Rules(rules) => self.patterns.extend(rules.iter().map(Rule::pattern)),
                Responder::Stream(_) => self.patterns.push(match_all()),
            }
            if self.proxy_auth.is_some() {
                self.patterns.push(self.auth_pattern);
            }
        } else if documents_only {
            self.patterns.push(self.auth_pattern);
        }
        let filters = self
            .patterns
            .into_iter()
            .map(Filter::new)
            .collect::<Result<_, _>>()?;
        Ok(Layer::new(filters, responder, self.proxy_auth))
    }

    /// Lazily push an empty pattern if none exists, so the stage/resource
    /// setters always have a target. Mirrors CDP's "missing fields default to
    /// match-all" semantics.
    fn ensure_pattern(&mut self) -> &mut RequestPattern {
        if self.patterns.is_empty() {
            self.patterns.push(RequestPattern::default());
        }
        self.patterns
            .last_mut()
            .expect("ensure_pattern pushed if empty")
    }

    /// Test-only accessor: number of registered rules. Used by the Task 5
    /// builder tests without exposing the rule list
    /// as public API.
    #[cfg(test)]
    pub(crate) fn rules_count(&self) -> usize {
        self.rules.len()
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt;
    use serde_json::{Value, json};
    use zendriver_transport::testing::MockConnection;

    use super::*;
    use crate::interceptor::testing::{applied, mock_interceptor, pause};

    /// The `Fetch.enable` parameters that `builder.start()` sends.
    async fn enable_params(mock: &mut MockConnection, builder: InterceptBuilder) -> Value {
        applied(mock, builder.start()).await.1
    }

    /// Register three rules (block + redirect + respond) on a fresh builder
    /// and assert the rule list grew to length 3. Verifies the chain wiring
    /// without touching Chrome.
    #[tokio::test]
    async fn three_rules_register_and_count() {
        let (_mock, conn, interceptor) = mock_interceptor();
        let builder = InterceptBuilder::new(&interceptor)
            .block("*/ads/*")
            .unwrap()
            .redirect("*/old/*", "https://example.com/new/")
            .unwrap()
            .respond(
                "*/api/health",
                200,
                vec![("content-type".into(), "application/json".into())],
                br#"{"ok":true}"#.to_vec(),
            )
            .unwrap();

        assert_eq!(builder.rules_count(), 3);
        conn.shutdown();
    }

    /// Without explicit [`pattern`](InterceptBuilder::pattern)s, `start()`
    /// pauses what the rules act on: each rule's URL pattern at its stage,
    /// and every URL for `block_hosts`.
    #[tokio::test]
    async fn start_derives_the_patterns_from_the_rules() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let hosts = Arc::new(HostMatcher::new(["evil.test".to_string()]));
        let builder = InterceptBuilder::new(&interceptor)
            .block("*/blocked/*")
            .unwrap()
            .modify_response("*/api/*", |_| crate::ResponseOverrides::default())
            .unwrap()
            .block_hosts(hosts);
        let params = enable_params(&mut mock, builder).await;
        assert_eq!(
            params["patterns"],
            json!([
                { "urlPattern": "*/blocked/*" },
                { "urlPattern": "*/api/*", "requestStage": "Response" },
                { "urlPattern": "*" },
            ])
        );
        conn.shutdown();
    }

    /// Regression guard for the "empty patterns with handleAuth" breakage: the
    /// proxy-auth-ONLY path (`handle_proxy_auth`, no rules, no explicit
    /// pattern) must still send a NON-empty `patterns` on `Fetch.enable`.
    /// Modern Chrome (~M132+) rejects `{ patterns: [], handleAuthRequests:
    /// true }` with CDP -32602 ("Can't specify empty patterns with handleAuth
    /// set"), which silently disables proxy-auth handling and 407s every
    /// navigation.
    #[tokio::test]
    async fn start_auth_only_sends_match_all_pattern_with_handle_proxy_auth() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let builder = InterceptBuilder::new(&interceptor).handle_proxy_auth("puser", "ppass");
        let params = enable_params(&mut mock, builder).await;
        assert_eq!(params["handleAuthRequests"], true);
        assert_eq!(params["patterns"], json!([{ "urlPattern": "*" }]));
        conn.shutdown();
    }

    /// The Document-scoped preset pauses only documents for the
    /// credentials (the difference from plain `handle_proxy_auth`), and
    /// leaves the rules' own patterns as they are.
    #[tokio::test]
    async fn start_auth_scoped_to_document_adds_a_document_pattern() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let builder = InterceptBuilder::new(&interceptor)
            .block("*/ads/*")
            .unwrap()
            .handle_proxy_auth_scoped_to_document("puser", "ppass");
        assert_eq!(builder.rules_count(), 1);
        let params = enable_params(&mut mock, builder).await;
        assert_eq!(params["handleAuthRequests"], true);
        assert_eq!(
            params["patterns"],
            json!([
                { "urlPattern": "*/ads/*" },
                { "urlPattern": "*", "resourceType": "Document" },
            ])
        );
        conn.shutdown();
    }

    /// On the `subscribe()` path: each `Fetch.requestPaused` event becomes
    /// a `PausedRequest` yielded from the stream, with the request payload
    /// decoded into [`RequestInfo`](crate::RequestInfo).
    #[tokio::test]
    async fn subscribe_yields_paused_request_per_event() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let (mut stream, _) =
            applied(&mut mock, InterceptBuilder::new(&interceptor).subscribe()).await;

        let headers = json!({ "request": {
            "url": "https://example.test/widget.json",
            "method": "GET",
            "headers": { "accept": "application/json" },
        }});
        pause(&mock, "REQ-1", "https://example.test/widget.json", headers).await;
        let paused = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("subscribe() stream did not yield within 2s")
            .expect("subscribe() stream closed before yielding");
        assert_eq!(paused.request_id, "REQ-1");
        assert_eq!(paused.request.url, "https://example.test/widget.json");
        assert_eq!(paused.request.method, "GET");
        assert_eq!(
            paused.request.headers,
            vec![("accept".to_string(), "application/json".to_string())]
        );
        assert!(
            paused.response.is_none(),
            "request-stage event has no response"
        );
        conn.shutdown();
    }

    /// `subscribe()` honors `handle_proxy_auth`: the multiplexer answers the
    /// challenges, so `handleAuthRequests` can be on.
    #[tokio::test]
    async fn subscribe_honors_credentials() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let builder = InterceptBuilder::new(&interceptor).handle_proxy_auth("puser", "ppass");
        let (_stream, params) = applied(&mut mock, builder.subscribe()).await;
        assert_eq!(params["handleAuthRequests"], true);
        conn.shutdown();
    }
}
