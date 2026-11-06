//! [`Interceptor`]: the one owner of the `Fetch` domain on a CDP session.
//!
//! Chrome keeps one `Fetch` configuration per DevTools session:
//! `Fetch.enable` replaces the session's patterns and `handleAuthRequests`,
//! `Fetch.disable` turns interception off for the whole session, and every
//! listener sees every `Fetch.requestPaused`. So one task per session, the
//! multiplexer, owns all of it.
//!
//! Each [`InterceptBuilder::start`] or [`InterceptBuilder::subscribe`] adds
//! a *layer*: its rules or its stream, the request patterns it pauses, and
//! the proxy credentials it was given, if any. Dropping the returned
//! [`InterceptHandle`] or [`InterceptStream`] removes only that layer. After
//! each change the multiplexer sends `Fetch.enable` with every layer's
//! patterns (and `handleAuthRequests` if any layer has credentials), or
//! `Fetch.disable` once no layer is left. `start` and `subscribe` return
//! after Chrome has replied, so the layer is in force from then on.
//!
//! Each paused request goes to the layers newest first. A layer considers it
//! only if one of its own patterns matches the request's URL, resource type
//! and stage. A stream layer takes every request it considers; a rule layer
//! takes one if one of its rules matches the URL and acts at that stage, and
//! otherwise leaves it to the next layer. A request that no layer takes is
//! continued unchanged. A proxy's challenge gets the credentials of the
//! newest layer that has some.
//!
//! If the multiplexer falls so far behind that the connection drops some
//! of its events, it disables `Fetch` and enables it again: a pause it
//! missed would otherwise stay paused, and `Fetch.disable` continues every
//! paused request, including those a stream holds (whose release then
//! fails). When the session's socket dies, is shut, or is replaced by a
//! reconnect, or the session detaches from its target, the layers are
//! dropped, which ends their streams.
//!
//! [`InterceptBuilder::start`]: crate::InterceptBuilder::start
//! [`InterceptBuilder::subscribe`]: crate::InterceptBuilder::subscribe

use std::collections::{HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures::{Stream, StreamExt};
use serde::Deserialize as _;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, warn};
use zendriver_transport::{AccountedRawEvent, RawEvent, SessionHandle};

use crate::builder::RequestPattern;
use crate::error::InterceptionError;
use crate::fetch::{
    RequestPausedEvent, build_request_info, build_response_info, continue_params,
    continue_response_params, fail_params, fulfill_params, parse_resource_type, serialize_pattern,
};
use crate::paused::PausedRequest;
use crate::rule::Rule;
use crate::types::{AbortReason, RequestOverrides, RequestStage, ResourceType};
use crate::url_pattern::UrlPattern;

/// The one owner of the `Fetch` domain on a CDP session: the layers added
/// through it share the session's `Fetch` configuration (see the module
/// docs).
///
/// Make exactly one per session. A second `Interceptor` on the same session
/// would replace this one's patterns with its own, and answer requests that
/// this one's layers should get. `zendriver`'s `Tab` holds its session's
/// interceptor, and `Tab::intercept` builds on it.
///
/// Clones share one interceptor. Nothing is sent to Chrome, and no task is
/// spawned, until the first layer is added.
#[derive(Clone, Debug)]
pub struct Interceptor {
    inner: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    session: SessionHandle,
    /// The multiplexer's command queue: made, and the multiplexer spawned,
    /// when the first layer is added.
    commands: OnceLock<mpsc::UnboundedSender<Command>>,
}

impl Interceptor {
    /// The interceptor for `session`. `session` must not have another
    /// `Interceptor`.
    ///
    /// ```no_run
    /// # async fn ex(session: zendriver_transport::SessionHandle)
    /// #   -> Result<(), zendriver_interception::InterceptionError> {
    /// use zendriver_interception::{InterceptBuilder, Interceptor};
    ///
    /// let interceptor = Interceptor::new(session);
    /// let _ads = InterceptBuilder::new(&interceptor).block("*/ads/*")?.start().await?;
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn new(session: SessionHandle) -> Self {
        Self {
            inner: Arc::new(Shared {
                session,
                commands: OnceLock::new(),
            }),
        }
    }

    /// Add `layer`, and return its handle once Chrome has applied it.
    pub(crate) async fn add(&self, layer: Layer) -> Result<InterceptHandle, InterceptionError> {
        let commands = self.inner.commands.get_or_init(|| {
            let (commands, received) = mpsc::unbounded_channel();
            tokio::spawn(run(self.inner.session.clone(), received));
            commands
        });
        // Made before the layer is sent, so a caller that stops waiting
        // drops it, which removes the layer again.
        let handle = InterceptHandle {
            layer: layer.id,
            commands: Some(commands.clone()),
        };
        let (done, applied) = oneshot::channel();
        commands
            .send(Command::Add(layer, done))
            .map_err(|_| InterceptionError::SubscriptionClosed)?;
        applied
            .await
            .map_err(|_| InterceptionError::SubscriptionClosed)??;
        Ok(handle)
    }
}

/// Keeps one layer of its session's interception in force: dropping the
/// handle removes the layer, and [`stop`](Self::stop) removes it and waits
/// until Chrome has applied the change.
#[derive(Debug)]
#[must_use = "the layer is removed when the handle is dropped — bind it to a variable to keep it in force"]
pub struct InterceptHandle {
    layer: LayerId,
    /// `None` after `stop`, and in a handle from `for_tests`.
    commands: Option<mpsc::UnboundedSender<Command>>,
}

impl InterceptHandle {
    /// Test-support constructor: a handle for no layer, whose drop does
    /// nothing. For downstream unit tests that need an `InterceptHandle` to
    /// fill a registry, without a session. Gated behind the `test-support`
    /// feature so production builds don't expose it.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn for_tests() -> Self {
        Self {
            layer: 0,
            commands: None,
        }
    }

    /// Remove the layer, and wait until Chrome has applied the change: the
    /// other layers' patterns, or `Fetch.disable` if no layer is left.
    ///
    /// # Errors
    ///
    /// [`InterceptionError::Call`] if Chrome refused the change, or the
    /// session's socket couldn't carry it. The layer is removed either way.
    /// (If the session's layers were already dropped, because its socket
    /// closed or it detached, this returns `Ok`.)
    pub async fn stop(mut self) -> Result<(), InterceptionError> {
        let Some(commands) = self.commands.take() else {
            return Ok(());
        };
        let (done, applied) = oneshot::channel();
        if commands
            .send(Command::Remove(self.layer, Some(done)))
            .is_err()
        {
            return Ok(());
        }
        applied.await.unwrap_or(Ok(()))
    }
}

impl Drop for InterceptHandle {
    fn drop(&mut self) {
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(Command::Remove(self.layer, None));
        }
    }
}

/// The paused requests that one stream layer takes, from
/// [`InterceptBuilder::subscribe`](crate::InterceptBuilder::subscribe).
///
/// Release each with one of [`PausedRequest`]'s methods. Dropping the
/// stream removes its layer, and continues each request it took that
/// wasn't read yet.
#[derive(Debug)]
#[must_use = "the layer is removed when the stream is dropped — bind it to a variable to keep it in force"]
pub struct InterceptStream {
    pub(crate) requests: mpsc::UnboundedReceiver<PausedRequest>,
    pub(crate) _layer: InterceptHandle,
}

impl Stream for InterceptStream {
    type Item = PausedRequest;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<PausedRequest>> {
        self.get_mut().requests.poll_recv(cx)
    }
}

/// Identifies a layer, for its handle to remove it.
pub(crate) type LayerId = u64;

/// One `start()` or `subscribe()` worth of interception.
pub(crate) struct Layer {
    id: LayerId,
    /// The requests this layer considers, each also one of the patterns
    /// sent with `Fetch.enable`.
    filters: Vec<Filter>,
    responder: Responder,
    /// The `(user, password)` to give a proxy that challenges.
    proxy_auth: Option<(String, String)>,
}

impl Layer {
    pub(crate) fn new(
        filters: Vec<Filter>,
        responder: Responder,
        proxy_auth: Option<(String, String)>,
    ) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            filters,
            responder,
            proxy_auth,
        }
    }

    /// Whether one of this layer's filters matches a request for `url`, of
    /// `resource_type`, paused at `stage`.
    fn considers(&self, url: &str, resource_type: ResourceType, stage: RequestStage) -> bool {
        self.filters
            .iter()
            .any(|f| f.matches(url, resource_type, stage))
    }
}

/// What takes the requests a layer considers.
pub(crate) enum Responder {
    /// The first of the rules that matches the request's URL and acts at
    /// its stage.
    Rules(Vec<Rule>),
    /// The stream from `subscribe()`.
    Stream(mpsc::UnboundedSender<PausedRequest>),
}

/// A request pattern, both as sent to Chrome and as matched here.
pub(crate) struct Filter {
    pattern: RequestPattern,
    url: UrlPattern,
}

impl Filter {
    /// # Errors
    ///
    /// [`InterceptionError::InvalidPattern`] if the URL pattern doesn't
    /// compile.
    pub(crate) fn new(pattern: RequestPattern) -> Result<Self, InterceptionError> {
        let url = UrlPattern::new(pattern.url_pattern.as_deref().unwrap_or("*"))?;
        Ok(Self { pattern, url })
    }

    /// Whether Chrome pauses a request for `url`, of `resource_type`, at
    /// `stage` for this pattern.
    fn matches(&self, url: &str, resource_type: ResourceType, stage: RequestStage) -> bool {
        self.pattern.request_stage.unwrap_or(RequestStage::Request) == stage
            && self
                .pattern
                .resource_type
                .is_none_or(|t| covers(t, resource_type))
            && self.url.matches(url)
    }
}

/// Whether a pattern for `pattern`-type requests covers one of `actual`
/// type. Chrome pauses XHR, Fetch and EventSource requests alike for a
/// pattern naming any of the three (they are one type to its loader; see
/// `AddInterceptedResourceType` in Chromium's `fetch_handler.cc`).
fn covers(pattern: ResourceType, actual: ResourceType) -> bool {
    let xhr_like = |t| {
        matches!(
            t,
            ResourceType::XHR | ResourceType::Fetch | ResourceType::EventSource
        )
    };
    pattern == actual || (xhr_like(pattern) && xhr_like(actual))
}

/// What a handle asks of the multiplexer.
pub(crate) enum Command {
    /// Add the layer, and say whether Chrome applied the new patterns.
    Add(Layer, oneshot::Sender<Result<(), InterceptionError>>),
    /// Remove the layer if it's there, and say (if asked) whether Chrome
    /// applied the change.
    Remove(
        LayerId,
        Option<oneshot::Sender<Result<(), InterceptionError>>>,
    ),
}

/// A session's `Fetch` events, with its connection's reports of lost
/// events and of the socket's end.
type FetchEvents = Pin<Box<dyn Stream<Item = AccountedRawEvent> + Send>>;

/// How long the multiplexer remembers having answered a proxy challenge with
/// the credentials. Chrome repeats a challenge whose credentials the proxy
/// rejected one round trip to the proxy later, so only recent answers
/// matter, and a long-lived tab doesn't grow the memory. (The Fetch domain
/// says nothing when a request finishes, so the memory can't forget a
/// request then.)
const AUTH_MEMORY: Duration = Duration::from_secs(60);

/// A proxy challenge: the request id, the challenger's origin, and the
/// auth scheme.
type Challenge = (String, String, String);

/// The proxy challenges answered with the credentials in the last
/// [`AUTH_MEMORY`].
#[derive(Debug, Default)]
struct Answered {
    /// Each challenge with when it was answered, oldest first.
    order: VecDeque<(Instant, Challenge)>,
    /// The same challenges, to look one up without a scan.
    keys: HashSet<Challenge>,
}

/// The `authChallengeResponse` to send for the `Fetch.authRequired` event
/// `event`.
///
/// - `proxy_auth`: the `(user, password)` to give a proxy, if any.
/// - `answered`: the challenges answered with them in the last
///   [`AUTH_MEMORY`] before `now`; updated here.
/// - `now`: the time of this event.
///
/// Credentials go only to a proxy (`authChallenge.source == "Proxy"`), and
/// once per challenge: Chrome repeats a challenge whose credentials the
/// proxy rejected, so it gets `CancelAuth` rather than a loop. A server's
/// challenge gets `Default`, so a site can't collect the credentials by
/// answering 401. A different proxy or scheme for the same request (PAC
/// failover, a scheme fallback) counts as a new challenge.
fn auth_response(
    proxy_auth: Option<&(String, String)>,
    event: &Value,
    answered: &mut Answered,
    now: Instant,
) -> Value {
    let challenge = &event["authChallenge"];
    let Some((user, pass)) = proxy_auth.filter(|_| challenge["source"] == "Proxy") else {
        return json!({ "response": "Default" });
    };
    while answered
        .order
        .front()
        .is_some_and(|(at, _)| now.saturating_duration_since(*at) > AUTH_MEMORY)
    {
        if let Some((_, key)) = answered.order.pop_front() {
            answered.keys.remove(&key);
        }
    }
    let field = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let key = (
        field(&event["requestId"]),
        field(&challenge["origin"]),
        field(&challenge["scheme"]),
    );
    if answered.keys.contains(&key) {
        warn!(?key, "interception: the proxy rejected the credentials");
        return json!({ "response": "CancelAuth" });
    }
    answered.keys.insert(key.clone());
    answered.order.push_back((now, key));
    json!({
        "response": "ProvideCredentials",
        "username": user,
        "password": pass,
    })
}

/// `session`'s `Fetch` events and its `Target.detachedFromTarget`, read
/// from the loss-accounted bus, which also says when events were lost and
/// when the socket died; they end when the socket is shut.
fn fetch_events(session: &SessionHandle) -> FetchEvents {
    let session_id = session.session_id().map(str::to_owned);
    let shut = session.connection().shutdown_token().cancelled_owned();
    let events = session.connection().subscribe_raw_accounted();
    let events = events.filter(move |ev| {
        let wanted = match ev {
            AccountedRawEvent::Event { event, .. } => {
                if event.method == "Target.detachedFromTarget" {
                    session_id.is_some()
                        && event.params["sessionId"].as_str() == session_id.as_deref()
                } else {
                    event.session_id == session_id && event.method.starts_with("Fetch.")
                }
            }
            _ => true,
        };
        std::future::ready(wanted)
    });
    Box::pin(events.take_until(shut))
}

/// The next of `events`, or `None` if they ended. Never ready while there
/// are no events to read.
async fn next_event(events: &mut Option<FetchEvents>) -> Option<AccountedRawEvent> {
    match events {
        Some(events) => events.next().await,
        None => std::future::pending().await,
    }
}

/// The command (method and params) with which `rule` releases the paused
/// request `ev`, or `None` if it can't take this request: a
/// `modify_response` rule and a failed response, which has no status or
/// headers to modify.
fn command(rule: &Rule, ev: &RequestPausedEvent) -> Option<(&'static str, Value)> {
    let id = ev.request_id.as_str();
    let unchanged = || {
        let params = continue_params(id, RequestOverrides::default());
        ("Fetch.continueRequest", params)
    };
    let command = match rule {
        Rule::Block { .. } | Rule::BlockHosts { .. } => (
            "Fetch.failRequest",
            fail_params(id, AbortReason::BlockedByClient),
        ),
        Rule::Redirect { to, .. } => {
            let overrides = RequestOverrides {
                url: Some(to.clone()),
                ..RequestOverrides::default()
            };
            ("Fetch.continueRequest", continue_params(id, overrides))
        }
        Rule::Respond {
            status,
            headers,
            body,
            ..
        } => (
            "Fetch.fulfillRequest",
            fulfill_params(id, *status, headers, body),
        ),
        Rule::Modify { modify, .. } => {
            let info = build_request_info(ev);
            match run_closure(|| modify(&info)) {
                Some(overrides) => ("Fetch.continueRequest", continue_params(id, overrides)),
                None => unchanged(),
            }
        }
        Rule::ModifyResponse { modify, .. } => {
            let info = build_response_info(ev)?;
            match run_closure(|| modify(&info)) {
                Some(overrides) => (
                    "Fetch.continueResponse",
                    continue_response_params(id, &info, overrides),
                ),
                None => unchanged(),
            }
        }
    };
    Some(command)
}

/// `f()`, or `None` (logged) if it panicked. A rule's closure is the
/// caller's code, and a panic left to unwind would end the multiplexer,
/// leaving the session's requests paused for good.
fn run_closure<T>(f: impl FnOnce() -> T) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => Some(value),
        Err(_) => {
            error!("interception: a rule's closure panicked; continuing the request unchanged");
            None
        }
    }
}

/// Send `method` with `params` to `session` without waiting, so the
/// multiplexer goes on to the next event.
fn send(session: &SessionHandle, method: &'static str, params: Value) {
    let session = session.clone();
    tokio::spawn(async move {
        if let Err(e) = session.call(method, params).await {
            warn!(error = %e, method, "interception: Chrome refused an answer to a challenge");
        }
    });
}

/// Release the paused request `request_id` with `method` and `params`,
/// without waiting (the multiplexer goes on to the next event). If Chrome
/// refuses them, the request is continued unchanged instead: one left
/// paused would stall its page for good.
fn release(session: &SessionHandle, request_id: String, method: &'static str, params: Value) {
    let session = session.clone();
    tokio::spawn(async move {
        let Err(e) = session.call(method, params).await else {
            return;
        };
        warn!(error = %e, method, "interception: Chrome refused a paused request's release; continuing it unchanged");
        let unchanged = continue_params(&request_id, RequestOverrides::default());
        if let Err(e) = session.call("Fetch.continueRequest", unchanged).await {
            debug!(error = %e, "interception: continuing the request failed too");
        }
    });
}

/// Make `session`'s `Fetch` configuration match `layers`.
async fn apply(session: &SessionHandle, layers: &[Layer]) -> Result<(), InterceptionError> {
    if layers.is_empty() {
        session.call("Fetch.disable", json!({})).await?;
        return Ok(());
    }
    let mut patterns: Vec<Value> = Vec::new();
    for filter in layers.iter().flat_map(|l| &l.filters) {
        let pattern = serialize_pattern(&filter.pattern);
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
    let handle_auth = layers.iter().any(|l| l.proxy_auth.is_some());
    session
        .call(
            "Fetch.enable",
            json!({ "patterns": patterns, "handleAuthRequests": handle_auth }),
        )
        .await?;
    Ok(())
}

/// The multiplexer's state; see the module docs.
struct Mux {
    session: SessionHandle,
    /// Oldest first.
    layers: Vec<Layer>,
    /// The session's `Fetch` events, subscribed to while any layer exists.
    events: Option<FetchEvents>,
    answered: Answered,
}

impl Mux {
    async fn obey(&mut self, command: Command) {
        match command {
            Command::Add(layer, done) => {
                // Subscribed before `Fetch.enable`, so no pause is missed.
                if self.events.is_none() {
                    self.events = Some(fetch_events(&self.session));
                }
                self.layers.push(layer);
                let applied = apply(&self.session, &self.layers).await;
                if applied.is_err() {
                    self.layers.pop();
                    if self.layers.is_empty() {
                        self.events = None;
                    }
                }
                let _ = done.send(applied);
            }
            Command::Remove(id, done) => {
                let mut applied = Ok(());
                if let Some(i) = self.layers.iter().position(|l| l.id == id) {
                    self.layers.remove(i);
                    applied = apply(&self.session, &self.layers).await;
                    if self.layers.is_empty() {
                        self.events = None;
                    }
                }
                match done {
                    Some(done) => {
                        let _ = done.send(applied);
                    }
                    None => {
                        if let Err(e) = applied {
                            debug!(error = %e, "interception: removing a dropped handle's layer failed");
                        }
                    }
                }
            }
        }
    }

    fn on_event(&mut self, event: RawEvent) {
        match event.method.as_str() {
            "Fetch.requestPaused" => self.dispatch(&event.params),
            "Fetch.authRequired" => self.authenticate(&event.params),
            "Target.detachedFromTarget" => self.end(),
            _ => {}
        }
    }

    /// Some events were lost, so a request paused then may have been missed,
    /// and would stay paused for good: disable `Fetch`, which continues
    /// every request still paused (including those a stream holds), and
    /// enable it again.
    async fn recover(&mut self, missed: u64) {
        warn!(
            missed,
            "interception: lost events; disabling Fetch, which continues every paused request, then enabling it again"
        );
        if let Err(e) = self.session.call("Fetch.disable", json!({})).await {
            warn!(error = %e, "interception: Fetch.disable failed");
        }
        if let Err(e) = apply(&self.session, &self.layers).await {
            warn!(error = %e, "interception: Fetch.enable failed; the layers are inert");
        }
    }

    /// The session is gone (its socket closed, or it detached): drop every
    /// layer, which ends their streams.
    fn end(&mut self) {
        debug!("interception: the session's events ended; dropping its layers");
        self.layers.clear();
        self.events = None;
    }

    /// Hand the `Fetch.requestPaused` event `params` to the newest layer
    /// that takes it (see the module docs).
    fn dispatch(&self, params: &Value) {
        let ev = match RequestPausedEvent::deserialize(params) {
            Ok(ev) => ev,
            Err(e) => {
                let Some(id) = params["requestId"].as_str() else {
                    warn!(error = %e, "interception: a Fetch.requestPaused event without requestId");
                    return;
                };
                warn!(error = %e, "interception: continuing a Fetch.requestPaused event it can't read");
                let unchanged = continue_params(id, RequestOverrides::default());
                release(&self.session, id.into(), "Fetch.continueRequest", unchanged);
                return;
            }
        };
        let stage = ev.stage();
        let resource_type = parse_resource_type(ev.resource_type.as_deref());
        for layer in self.layers.iter().rev() {
            if !layer.considers(&ev.request.url, resource_type, stage) {
                continue;
            }
            match &layer.responder {
                Responder::Stream(requests) => {
                    if requests.is_closed() {
                        continue;
                    }
                    let paused = PausedRequest::new(
                        ev.request_id.clone(),
                        build_request_info(&ev),
                        build_response_info(&ev),
                        self.session.clone(),
                    );
                    // If the stream was dropped since, the request comes
                    // back, and its drop continues it.
                    let _ = requests.send(paused);
                    return;
                }
                Responder::Rules(rules) => {
                    let url = &ev.request.url;
                    let taken = rules
                        .iter()
                        .filter(|r| r.acts_at(stage) && r.matches(url))
                        .find_map(|r| command(r, &ev));
                    let Some((method, params)) = taken else {
                        continue;
                    };
                    release(&self.session, ev.request_id, method, params);
                    return;
                }
            }
        }
        let params = continue_params(&ev.request_id, RequestOverrides::default());
        release(
            &self.session,
            ev.request_id,
            "Fetch.continueRequest",
            params,
        );
    }

    /// Answer the `Fetch.authRequired` event `params`, with the credentials
    /// of the newest layer that has some.
    fn authenticate(&mut self, params: &Value) {
        let Some(request_id) = params["requestId"].as_str() else {
            warn!("interception: Fetch.authRequired without requestId");
            return;
        };
        let credentials = self.layers.iter().rev().find_map(|l| l.proxy_auth.as_ref());
        let response = auth_response(credentials, params, &mut self.answered, Instant::now());
        send(
            &self.session,
            "Fetch.continueWithAuth",
            json!({ "requestId": request_id, "authChallengeResponse": response }),
        );
    }
}

/// Run `session`'s multiplexer until every sender of `commands` is gone.
async fn run(session: SessionHandle, mut commands: mpsc::UnboundedReceiver<Command>) {
    let mut mux = Mux {
        session,
        layers: Vec::new(),
        events: None,
        answered: Answered::default(),
    };
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    break;
                };
                mux.obey(command).await;
            }
            event = next_event(&mut mux.events) => match event {
                Some(AccountedRawEvent::Event { event, .. }) => mux.on_event(event),
                Some(AccountedRawEvent::Lagged { missed, .. }) => mux.recover(missed).await,
                // A reconnect gives the connection a new socket, where this
                // session doesn't exist.
                Some(AccountedRawEvent::Reconnected { .. } | AccountedRawEvent::Disconnected { .. })
                | None => mux.end(),
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
pub(crate) mod testing {
    //! Helpers for driving an [`Interceptor`] against a mock Chrome.

    use std::future::Future;
    use std::time::Duration;

    use serde_json::{Value, json};
    use zendriver_transport::testing::MockConnection;
    use zendriver_transport::{Connection, SessionHandle};

    use super::Interceptor;
    use crate::error::InterceptionError;

    /// How long a test waits for a command it expects.
    const BUDGET: Duration = Duration::from_secs(2);

    /// A mock browser connection, and the interceptor of its session `S1`.
    pub(crate) fn mock_interceptor() -> (MockConnection, Connection, Interceptor) {
        let (mock, conn) = MockConnection::pair();
        let interceptor = Interceptor::new(SessionHandle::new(conn.clone(), "S1"));
        (mock, conn, interceptor)
    }

    /// The parameters of the next `method` command sent (skipping others),
    /// answered with `{}`.
    pub(crate) async fn answered(mock: &mut MockConnection, method: &str) -> Value {
        let id = tokio::time::timeout(BUDGET, mock.expect_cmd(method))
            .await
            .unwrap_or_else(|_| panic!("no {method} within {BUDGET:?}"));
        let params = mock.last_sent()["params"].clone();
        mock.reply(id, json!({})).await;
        params
    }

    /// Each command sent until none comes for 300 ms, as its method and
    /// parameters, answered with `{}`.
    pub(crate) async fn sent_until_quiet(mock: &mut MockConnection) -> Vec<(String, Value)> {
        let mut sent = Vec::new();
        while let Some((method, id)) = mock.recv_cmd_timeout(Duration::from_millis(300)).await {
            sent.push((method, mock.last_sent()["params"].clone()));
            mock.reply(id, json!({})).await;
        }
        sent
    }

    /// What `add` (a `start()` or `subscribe()`) returns, run while the mock
    /// answers its `Fetch.enable`; and that command's parameters.
    pub(crate) async fn applied<T: Send + 'static>(
        mock: &mut MockConnection,
        add: impl Future<Output = Result<T, InterceptionError>> + Send + 'static,
    ) -> (T, Value) {
        let add = tokio::spawn(add);
        let params = answered(mock, "Fetch.enable").await;
        (add.await.unwrap().unwrap(), params)
    }

    /// Have the mock pause request `id` for `url`, with `fields` (e.g.
    /// `resourceType`, `responseStatusCode`) added to the event.
    pub(crate) async fn pause(mock: &MockConnection, id: &str, url: &str, fields: Value) {
        let mut event = json!({
            "requestId": id,
            "request": { "url": url, "method": "GET", "headers": {} },
        });
        if let (Some(event), Some(fields)) = (event.as_object_mut(), fields.as_object()) {
            event.extend(fields.clone());
        }
        mock.emit_event_for_session("Fetch.requestPaused", event, "S1")
            .await;
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use zendriver_transport::testing::MockConnection;

    use super::testing::{answered, applied, mock_interceptor, pause, sent_until_quiet};
    use super::*;
    use crate::InterceptBuilder;
    use crate::types::ResponseOverrides;

    /// A rule layer fails the request its rule matches; `stop` disables
    /// `Fetch` (the last layer is gone) and returns after Chrome replied.
    #[tokio::test]
    async fn a_rule_layer_fails_a_matching_request_and_stop_disables_fetch() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let builder = InterceptBuilder::new(&interceptor)
            .block("*/blocked/*")
            .unwrap();
        let (handle, enable) = applied(&mut mock, builder.start()).await;
        assert_eq!(enable["handleAuthRequests"], false);
        assert_eq!(enable["patterns"], json!([{ "urlPattern": "*/blocked/*" }]));

        pause(&mock, "R1", "https://a.test/blocked/x.png", json!({})).await;
        let failed = answered(&mut mock, "Fetch.failRequest").await;
        assert_eq!(
            failed,
            json!({ "requestId": "R1", "errorReason": "BlockedByClient" })
        );

        let stop = tokio::spawn(handle.stop());
        answered(&mut mock, "Fetch.disable").await;
        stop.await.unwrap().unwrap();
        conn.shutdown();
    }

    /// `start` returns only after Chrome has replied to `Fetch.enable`.
    #[tokio::test]
    async fn start_returns_once_chrome_has_applied_the_patterns() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let mut start = tokio::spawn(
            InterceptBuilder::new(&interceptor)
                .block("*")
                .unwrap()
                .start(),
        );
        let id = mock.expect_cmd("Fetch.enable").await;
        let early = tokio::time::timeout(Duration::from_millis(100), &mut start).await;
        assert!(early.is_err(), "start returned before Chrome replied");
        mock.reply(id, json!({})).await;
        let _handle = start.await.unwrap().unwrap();
        conn.shutdown();
    }

    /// Two layers share one `Fetch` configuration: each change sends the
    /// union of their patterns, a pause goes to the newest layer with a
    /// matching pattern and rule, and one that no layer takes is continued.
    /// Removing a layer leaves the other in force; removing the last
    /// disables `Fetch`.
    #[tokio::test]
    async fn layers_share_one_fetch_configuration_newest_first() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let older = InterceptBuilder::new(&interceptor)
            .block("*/a/*")
            .unwrap()
            .handle_proxy_auth("u", "p");
        let (older, enable) = applied(&mut mock, older.start()).await;
        assert_eq!(enable["handleAuthRequests"], true);

        let newer = InterceptBuilder::new(&interceptor)
            .pattern("*/b/*")
            .respond("*", 204, Vec::new(), Vec::new())
            .unwrap();
        let (newer, enable) = applied(&mut mock, newer.start()).await;
        assert_eq!(
            enable,
            json!({
                "patterns": [
                    { "urlPattern": "*/a/*" },
                    { "urlPattern": "*" },
                    { "urlPattern": "*/b/*" },
                ],
                "handleAuthRequests": true,
            })
        );

        pause(&mock, "R1", "https://x.test/b/1", json!({})).await;
        assert_eq!(
            answered(&mut mock, "Fetch.fulfillRequest").await["requestId"],
            "R1"
        );
        // The newer layer's pattern doesn't cover /a/, so the older one gets it.
        pause(&mock, "R2", "https://x.test/a/1", json!({})).await;
        assert_eq!(
            answered(&mut mock, "Fetch.failRequest").await["requestId"],
            "R2"
        );
        pause(&mock, "R3", "https://x.test/c/1", json!({})).await;
        assert_eq!(
            answered(&mut mock, "Fetch.continueRequest").await,
            json!({ "requestId": "R3" })
        );

        drop(newer);
        assert_eq!(
            answered(&mut mock, "Fetch.enable").await["patterns"],
            json!([{ "urlPattern": "*/a/*" }, { "urlPattern": "*" }])
        );
        pause(&mock, "R4", "https://x.test/b/1", json!({})).await;
        assert_eq!(
            answered(&mut mock, "Fetch.continueRequest").await["requestId"],
            "R4"
        );

        drop(older);
        answered(&mut mock, "Fetch.disable").await;
        conn.shutdown();
    }

    /// A stream layer takes every request its patterns pause, ahead of an
    /// older rule layer. A request read and dropped unreleased is
    /// continued, and so is one never read when the stream is dropped,
    /// which leaves the rule layer in force.
    #[tokio::test]
    async fn a_stream_takes_what_it_pauses_and_its_drop_continues_the_unread() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let rules = InterceptBuilder::new(&interceptor).block("*").unwrap();
        let (_rules, _) = applied(&mut mock, rules.start()).await;
        let stream = InterceptBuilder::new(&interceptor).pattern("*/s/*");
        let (mut stream, _) = applied(&mut mock, stream.subscribe()).await;

        pause(
            &mock,
            "R1",
            "https://x.test/s/1",
            json!({ "resourceType": "XHR" }),
        )
        .await;
        pause(&mock, "R2", "https://x.test/s/2", json!({})).await;
        let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.request.url, "https://x.test/s/1");
        assert_eq!(first.request.resource_type, ResourceType::XHR);
        drop(first);
        drop(stream);
        let sent = sent_until_quiet(&mut mock).await;
        for id in ["R1", "R2"] {
            let continued = (
                "Fetch.continueRequest".to_string(),
                json!({ "requestId": id }),
            );
            assert!(sent.contains(&continued), "{id} not continued: {sent:?}");
        }
        assert!(
            sent.iter().any(
                |(m, p)| m == "Fetch.enable" && p["patterns"] == json!([{ "urlPattern": "*" }])
            ),
            "{sent:?}"
        );
        conn.shutdown();
    }

    /// A layer whose pattern names XHR requests considers a `fetch()`'s too:
    /// Chrome pauses both for it.
    #[tokio::test]
    async fn an_xhr_pattern_covers_fetch_requests() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let builder = InterceptBuilder::new(&interceptor)
            .pattern("*")
            .resource(ResourceType::XHR)
            .block("*")
            .unwrap();
        let (_handle, _) = applied(&mut mock, builder.start()).await;
        pause(
            &mock,
            "R1",
            "https://x.test/api",
            json!({ "resourceType": "Fetch" }),
        )
        .await;
        assert_eq!(
            answered(&mut mock, "Fetch.failRequest").await["requestId"],
            "R1"
        );
        conn.shutdown();
    }

    /// A rule that can't act at the stage a request is paused at leaves it
    /// to the next rule: `modify_request` is skipped once there is a
    /// response, and `modify_response` before.
    #[tokio::test]
    async fn rules_act_only_at_their_stage() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let builder = InterceptBuilder::new(&interceptor)
            .modify_request("*/req/*", |_| RequestOverrides::default())
            .unwrap()
            .modify_response("*", |_| ResponseOverrides {
                status: Some(203),
                ..ResponseOverrides::default()
            })
            .unwrap()
            .block("*/ads/*")
            .unwrap();
        let (_handle, enable) = applied(&mut mock, builder.start()).await;
        assert_eq!(
            enable["patterns"],
            json!([
                { "urlPattern": "*/req/*" },
                { "urlPattern": "*", "requestStage": "Response" },
                { "urlPattern": "*/ads/*" },
            ])
        );

        pause(&mock, "R1", "https://x.test/ads/1", json!({})).await;
        assert_eq!(
            answered(&mut mock, "Fetch.failRequest").await["requestId"],
            "R1"
        );
        let response = json!({ "responseStatusCode": 200 });
        pause(&mock, "R2", "https://x.test/req/1", response).await;
        let continued = answered(&mut mock, "Fetch.continueResponse").await;
        assert_eq!(continued["requestId"], "R2");
        assert_eq!(continued["responseCode"], 203);
        conn.shutdown();
    }

    /// A release that Chrome refuses is followed by a plain
    /// `continueRequest`, and a pause whose event doesn't decode is
    /// continued by its `requestId`: neither request is left paused.
    #[tokio::test]
    async fn a_refused_or_unreadable_pause_is_continued() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let builder = InterceptBuilder::new(&interceptor)
            .respond(
                "*",
                200,
                vec![("bad header".into(), "x".into())],
                Vec::new(),
            )
            .unwrap();
        let (_handle, _) = applied(&mut mock, builder.start()).await;

        pause(&mock, "R1", "https://x.test/", json!({})).await;
        let id = mock.expect_cmd("Fetch.fulfillRequest").await;
        mock.reply_err(id, -32602, "Invalid header: bad header")
            .await;
        assert_eq!(
            answered(&mut mock, "Fetch.continueRequest").await,
            json!({ "requestId": "R1" })
        );

        let unreadable = json!({ "requestId": "R2", "request": "not an object" });
        mock.emit_event_for_session("Fetch.requestPaused", unreadable, "S1")
            .await;
        assert_eq!(
            answered(&mut mock, "Fetch.continueRequest").await,
            json!({ "requestId": "R2" })
        );
        conn.shutdown();
    }

    /// Lost events make the multiplexer disable `Fetch` (which continues
    /// any pause it missed) and enable it again with every layer's
    /// patterns.
    #[tokio::test]
    async fn lost_events_disable_fetch_and_enable_it_again() {
        let (mut mock, conn) = MockConnection::pair_with_accounted_capacity(4);
        let interceptor = Interceptor::new(SessionHandle::new(conn.clone(), "S1"));
        let first = InterceptBuilder::new(&interceptor).block("*/a/*").unwrap();
        let (_first, _) = applied(&mut mock, first.start()).await;

        // While the multiplexer waits for Chrome, more events arrive than
        // its bus holds.
        let second = InterceptBuilder::new(&interceptor).block("*/b/*").unwrap();
        let second = tokio::spawn(second.start());
        let id = mock.expect_cmd("Fetch.enable").await;
        for _ in 0..10 {
            mock.emit_event_for_session("Network.dataReceived", json!({}), "S1")
                .await;
        }
        mock.reply(id, json!({})).await;
        let _second = second.await.unwrap().unwrap();

        let sent = sent_until_quiet(&mut mock).await;
        let methods: Vec<&str> = sent.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(methods, ["Fetch.disable", "Fetch.enable"], "{sent:?}");
        assert_eq!(
            sent[1].1["patterns"],
            json!([{ "urlPattern": "*/a/*" }, { "urlPattern": "*/b/*" }])
        );
        conn.shutdown();
    }

    /// A stream ends when its session detaches from its target, or when the
    /// session's socket is shut.
    #[tokio::test]
    async fn a_stream_ends_with_its_session() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let (mut stream, _) =
            applied(&mut mock, InterceptBuilder::new(&interceptor).subscribe()).await;
        mock.emit_event("Target.detachedFromTarget", json!({ "sessionId": "S1" }))
            .await;
        let next = tokio::time::timeout(Duration::from_secs(2), stream.next()).await;
        assert!(matches!(next, Ok(None)), "the stream didn't end on detach");

        let (mut stream, _) =
            applied(&mut mock, InterceptBuilder::new(&interceptor).subscribe()).await;
        conn.shutdown();
        let next = tokio::time::timeout(Duration::from_secs(2), stream.next()).await;
        assert!(
            matches!(next, Ok(None)),
            "the stream didn't end on shutdown"
        );
    }

    /// A failed response (no status, no headers) isn't taken by a
    /// `modify_response` rule, so a later rule of the layer gets it.
    #[tokio::test]
    async fn a_failed_response_goes_past_modify_response() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let builder = InterceptBuilder::new(&interceptor)
            .pattern("*")
            .at_response()
            .modify_response("*", |_| ResponseOverrides::default())
            .unwrap()
            .block("*")
            .unwrap();
        let (_handle, _) = applied(&mut mock, builder.start()).await;
        let failed = json!({ "responseErrorReason": "ConnectionRefused" });
        pause(&mock, "R1", "https://x.test/", failed).await;
        assert_eq!(
            answered(&mut mock, "Fetch.failRequest").await["requestId"],
            "R1"
        );
        conn.shutdown();
    }

    /// A rule's closure that panics leaves the request continued, unchanged,
    /// and the multiplexer running.
    #[tokio::test]
    async fn a_panicking_closure_continues_the_request() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let builder = InterceptBuilder::new(&interceptor)
            .pattern("*")
            .at_response()
            .modify_response("*", |_| -> ResponseOverrides {
                panic!("a bug in the closure")
            })
            .unwrap();
        let (_handle, _) = applied(&mut mock, builder.start()).await;
        for id in ["R1", "R2"] {
            pause(
                &mock,
                id,
                "https://x.test/",
                json!({ "responseStatusCode": 200 }),
            )
            .await;
            assert_eq!(
                answered(&mut mock, "Fetch.continueRequest").await,
                json!({ "requestId": id })
            );
        }
        conn.shutdown();
    }

    /// A proxy's challenge gets the newest credentials; a session whose
    /// layers have none answers `Default`.
    #[tokio::test]
    async fn a_proxy_challenge_gets_the_newest_credentials() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let challenge = |id: &str| {
            json!({
                "requestId": id,
                "authChallenge": { "source": "Proxy", "origin": "http://proxy.test", "scheme": "basic" },
            })
        };
        let plain = InterceptBuilder::new(&interceptor).block("*/x").unwrap();
        let (_plain, _) = applied(&mut mock, plain.start()).await;
        mock.emit_event_for_session("Fetch.authRequired", challenge("A1"), "S1")
            .await;
        let answer = answered(&mut mock, "Fetch.continueWithAuth").await;
        assert_eq!(answer["authChallengeResponse"]["response"], "Default");

        let mut handles = Vec::new();
        for user in ["old", "new"] {
            let auth = InterceptBuilder::new(&interceptor).handle_proxy_auth(user, "p");
            handles.push(applied(&mut mock, auth.start()).await.0);
        }
        mock.emit_event_for_session("Fetch.authRequired", challenge("A2"), "S1")
            .await;
        let answer = answered(&mut mock, "Fetch.continueWithAuth").await;
        assert_eq!(answer["requestId"], "A2");
        assert_eq!(
            answer["authChallengeResponse"]["response"],
            "ProvideCredentials"
        );
        assert_eq!(answer["authChallengeResponse"]["username"], "new");
        conn.shutdown();
    }

    /// Proxy credentials go to a proxy's challenge once: a repeat (the
    /// proxy rejected them) gets `CancelAuth`, another proxy or scheme for
    /// the same request gets them again, and a server's challenge or a
    /// tab without credentials gets `Default`.
    #[test]
    fn auth_response_gives_credentials_to_each_proxy_challenge_once() {
        let creds = ("u".to_string(), "p".to_string());
        let mut answered = Answered::default();
        let now = Instant::now();
        let event = |source: &str, origin: &str| {
            json!({
                "requestId": "R1",
                "authChallenge": { "source": source, "origin": origin, "scheme": "basic" },
            })
        };
        let mut respond = |auth, event: Value| {
            auth_response(auth, &event, &mut answered, now)["response"].clone()
        };
        assert_eq!(
            respond(Some(&creds), event("Proxy", "a")),
            "ProvideCredentials"
        );
        assert_eq!(respond(Some(&creds), event("Proxy", "a")), "CancelAuth");
        assert_eq!(
            respond(Some(&creds), event("Proxy", "b")),
            "ProvideCredentials"
        );
        assert_eq!(respond(Some(&creds), event("Server", "c")), "Default");
        assert_eq!(respond(None, event("Proxy", "d")), "Default");
        assert_eq!(answered.order.len(), 2);
    }

    /// The multiplexer remembers every challenge it answered in the last
    /// [`AUTH_MEMORY`], however many others it answered since, and then
    /// forgets it.
    #[test]
    fn auth_response_remembers_challenges_for_a_while() {
        let creds = ("u".to_string(), "p".to_string());
        let mut answered = Answered::default();
        let event = |request: usize| {
            json!({
                "requestId": format!("R{request}"),
                "authChallenge": { "source": "Proxy", "origin": "http://p:1", "scheme": "basic" },
            })
        };
        let start = Instant::now();
        let mut respond = |request, at| {
            auth_response(Some(&creds), &event(request), &mut answered, at)["response"].clone()
        };
        for request in 0..1000 {
            assert_eq!(respond(request, start), "ProvideCredentials");
        }
        assert_eq!(respond(0, start + AUTH_MEMORY), "CancelAuth");
        let later = start + AUTH_MEMORY + Duration::from_secs(1);
        assert_eq!(respond(0, later), "ProvideCredentials");
        assert_eq!(answered.order.len(), 1);
        assert_eq!(answered.keys.len(), 1);
    }
}
