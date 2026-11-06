//! [`Interceptor`]: the one owner of the `Fetch` domain on a CDP session.
//!
//! A session's interception is one ordered list of entries, each one
//! [`Rule`] or one stream ([`Interceptor::subscribe`]), and the session's
//! proxy credentials ([`ProxyAuth`]), outside the list. How a paused request
//! is decided:
//!
//! 1. `insert`, `subscribe` and `move_rule` put entries where their
//!    [`Position`] says in the list as it is at that moment; `insert` puts
//!    its rules in consecutively, in their order. Removing an entry keeps
//!    the others' order. A `Before(id)` / `After(id)` naming no entry is an
//!    error.
//! 2. Chrome pauses a request at the Request stage when some
//!    [`Request`](PauseKind::Request) entry fits its URL and resource
//!    type, or while proxy credentials are set (every request, or only
//!    documents under [`AuthPause::DocumentsOnly`]), and at the Response
//!    stage when some [`ResponseHeaders`](PauseKind::ResponseHeaders) or
//!    [`ResponseError`](PauseKind::ResponseError) entry fits.
//! 3. A pause's kind is `Request`, `ResponseHeaders` (Chrome has a status,
//!    4xx and 5xx included) or `ResponseError` (the request failed before
//!    any status).
//! 4. The first entry, front to back, whose [`Match`] fits the pause (its
//!    kind, URL and resource type) decides it: its action answers Chrome,
//!    and no later entry runs. A pause no entry fits is continued
//!    unchanged.
//! 5. Every pause walks the list anew: deciding a Request pause says
//!    nothing about the Response pause, and each 3xx hop is a new request
//!    to Chrome.
//! 6. A stream entry owns every pause it decides: the consumer answers each
//!    through its [`PausedRequest`].
//! 7. A proxy's challenge gets the session's credentials once; a repeated
//!    challenge is cancelled; a server's challenge always gets Chrome's
//!    default.
//!
//! If the multiplexer falls so far behind that the connection drops some
//! of its events, it disables `Fetch` and enables it again: a pause it
//! missed would otherwise stay paused, and `Fetch.disable` continues every
//! paused request, including those a stream holds (whose release then
//! fails). When the session's socket dies, is shut, or is replaced by a
//! reconnect, or the session detaches from its target, the entries are
//! dropped, which ends their streams.

use std::collections::{HashSet, VecDeque};
use std::fmt;
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

use crate::error::InterceptionError;
use crate::fetch::{
    RequestPausedEvent, build_request_info, build_response_info, continue_params,
    continue_response_params, fail_params, fulfill_params, parse_resource_type,
};
use crate::paused::PausedRequest;
use crate::rule::{Action, Match, Rule, UrlMatch};
use crate::types::{AbortReason, PauseKind, RequestOverrides, ResourceType};

/// Identifies an entry of a session's interception. Never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RuleId(u64);

impl RuleId {
    /// An id no entry has had.
    fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for RuleId {
    type Err = std::num::ParseIntError;

    /// The id that [`Display`](fmt::Display) wrote as `s`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(Self)
    }
}

/// Where [`Interceptor::insert`], [`Interceptor::subscribe`] and
/// [`Interceptor::move_rule`] put entries, in the list as it is at that
/// moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    /// Ahead of every entry.
    First,
    /// Behind every entry.
    Last,
    /// Just ahead of that entry.
    Before(RuleId),
    /// Just behind that entry.
    After(RuleId),
}

/// A session's proxy credentials, and which requests Chrome pauses for
/// them: a proxy's challenge surfaces only for a request paused at the
/// Request stage.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyAuth {
    /// The user name given to the proxy.
    pub user: String,
    /// The password given to the proxy.
    pub password: String,
    /// Which requests Chrome pauses so that their challenges surface.
    pub pause: AuthPause,
}

// Hand-written so that the password stays out of logs.
impl fmt::Debug for ProxyAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyAuth")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("pause", &self.pause)
            .finish()
    }
}

/// Which requests Chrome pauses for the proxy credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthPause {
    /// Every request: every challenge surfaces.
    EveryRequest,
    /// Only document requests (the page's and its frames' navigations),
    /// for fewer round trips: a 407 on another request's connection is
    /// missed, unless an entry pauses that request.
    DocumentsOnly,
}

/// A session's interception at one moment.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// The entries, in the order a paused request walks them.
    pub entries: Vec<Entry>,
    /// Which requests Chrome pauses for the proxy credentials, or `None`
    /// without credentials.
    pub proxy_auth: Option<AuthPause>,
}

/// One entry of a [`Snapshot`].
#[derive(Clone, Debug)]
pub struct Entry {
    /// The entry's id, for `Position::Before` / `After`, `move_rule` and
    /// `remove`.
    pub id: RuleId,
    /// The rule's label, if it was given one.
    pub label: Option<String>,
    /// The kind of pause the entry fits.
    pub pause: PauseKind,
    /// The URLs the entry fits.
    pub url: UrlInfo,
    /// The resource type the entry fits, or `None` for any.
    pub resource_type: Option<ResourceType>,
    /// What the entry does with a request it decides.
    pub action: ActionKind,
}

/// The URLs an [`Entry`] fits.
#[derive(Clone, Debug)]
pub enum UrlInfo {
    /// Those a CDP wildcard pattern matches.
    Glob(String),
    /// Those whose host is in a set of `count` hosts (or is a subdomain of
    /// one), listed in `hosts` when the snapshot was asked for them.
    Hosts {
        /// The number of hosts in the set.
        count: usize,
        /// The hosts, sorted, or `None` if not asked for.
        hosts: Option<Vec<String>>,
    },
}

/// What an [`Entry`] does with a paused request it decides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionKind {
    /// [`Action::Continue`].
    Continue,
    /// [`Action::Block`], with its reason.
    Block(AbortReason),
    /// [`Action::Respond`], with its status.
    Respond {
        /// The status of the response it answers with.
        status: u16,
    },
    /// [`Action::RewriteUrl`], with its URL.
    RewriteUrl(String),
    /// [`Action::ModifyRequest`].
    ModifyRequest,
    /// [`Action::ModifyResponse`].
    ModifyResponse,
    /// A stream, from [`Interceptor::subscribe`].
    Stream,
}

/// The one owner of the `Fetch` domain on a CDP session: the entries and
/// credentials set through it make up the session's interception (see the
/// module docs).
///
/// Make exactly one per session. A second `Interceptor` on the same session
/// would replace this one's patterns with its own, and answer requests that
/// this one's entries should decide. `zendriver`'s `Tab` holds its
/// session's interceptor (`Tab::interceptor`).
///
/// Clones share one interceptor. Nothing is sent to Chrome, and no task is
/// spawned, until the first change.
#[derive(Clone, Debug)]
pub struct Interceptor {
    inner: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    session: SessionHandle,
    /// The multiplexer's command queue: made, and the multiplexer spawned,
    /// at the first change.
    commands: OnceLock<mpsc::UnboundedSender<Command>>,
}

impl Interceptor {
    /// The interceptor for `session`. `session` must not have another
    /// `Interceptor`.
    ///
    /// ```no_run
    /// # async fn ex(session: zendriver_transport::SessionHandle)
    /// #   -> Result<(), zendriver_interception::InterceptionError> {
    /// use zendriver_interception::{Action, Interceptor, Match, Position, Rule};
    ///
    /// let interceptor = Interceptor::new(session);
    /// let blocked = zendriver_interception::AbortReason::BlockedByClient;
    /// let ads = Rule::new(Match::url("*/ads/*")?, Action::Block(blocked))?;
    /// let _ads = interceptor.insert(vec![ads], Position::Last).await?;
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

    /// Insert `rules` at `at`, consecutively and in their order, and return
    /// once Chrome pauses the requests they fit. They stay in until the
    /// returned handle is dropped or stopped (or `remove` removes them).
    ///
    /// # Errors
    ///
    /// [`InterceptionError::NoRules`] for no rules,
    /// [`InterceptionError::NoSuchRule`] for a `Before` / `After` naming no
    /// entry, and [`InterceptionError::Call`] if Chrome refused the patterns
    /// or the session is gone; nothing is inserted then.
    pub async fn insert(
        &self,
        rules: Vec<Rule>,
        at: Position,
    ) -> Result<InterceptHandle, InterceptionError> {
        if rules.is_empty() {
            return Err(InterceptionError::NoRules);
        }
        let slots: Vec<Slot> = rules
            .into_iter()
            .map(|rule| Slot {
                id: RuleId::next(),
                label: rule.label,
                when: rule.when,
                responder: Responder::Rule(rule.action),
            })
            .collect();
        // Made before the entries are sent, so a caller that stops waiting
        // drops it, which removes them again.
        let handle = self.handle(slots.iter().map(|s| s.id).collect());
        self.ask(|done| Command::Insert(slots, at, done)).await??;
        Ok(handle)
    }

    /// Insert at `at` an entry that hands each paused request `when` fits
    /// to the returned stream, and return once Chrome pauses them.
    ///
    /// # Errors
    ///
    /// As for [`insert`](Self::insert).
    pub async fn subscribe(
        &self,
        when: Match,
        at: Position,
    ) -> Result<InterceptStream, InterceptionError> {
        let (requests, received) = mpsc::unbounded_channel();
        let slot = Slot {
            id: RuleId::next(),
            label: None,
            when,
            responder: Responder::Stream(requests),
        };
        let handle = self.handle(vec![slot.id]);
        self.ask(|done| Command::Insert(vec![slot], at, done))
            .await??;
        Ok(InterceptStream {
            requests: received,
            handle,
        })
    }

    /// Move entry `id` to `at` (a stream keeps the pauses it holds).
    ///
    /// # Errors
    ///
    /// [`InterceptionError::NoSuchRule`] if `id`, or the entry `at` names,
    /// isn't in the list; nothing moves then.
    pub async fn move_rule(&self, id: RuleId, at: Position) -> Result<(), InterceptionError> {
        self.ask(|done| Command::Move(id, at, done)).await?
    }

    /// Remove entry `id`, whoever inserted it, and return once Chrome has
    /// applied the change.
    ///
    /// # Errors
    ///
    /// [`InterceptionError::NoSuchRule`] if `id` isn't in the list, and
    /// [`InterceptionError::Call`] if Chrome refused the change (the entry
    /// is removed anyway).
    pub async fn remove(&self, id: RuleId) -> Result<(), InterceptionError> {
        self.ask(|done| Command::Remove {
            ids: vec![id],
            strict: true,
            done: Some(done),
        })
        .await?
    }

    /// Set the session's proxy credentials to `auth` (`None` for none),
    /// replacing the previous ones, and return once Chrome has applied it.
    ///
    /// # Errors
    ///
    /// [`InterceptionError::Call`] if Chrome refused it; the previous
    /// credentials stay then.
    pub async fn set_proxy_auth(&self, auth: Option<ProxyAuth>) -> Result<(), InterceptionError> {
        self.ask(|done| Command::SetProxyAuth(auth, done)).await?
    }

    /// The session's interception now; with `include_hosts`, a host set's
    /// hosts too (not only how many).
    ///
    /// # Errors
    ///
    /// [`InterceptionError::SubscriptionClosed`] if the interception task
    /// is gone.
    pub async fn snapshot(&self, include_hosts: bool) -> Result<Snapshot, InterceptionError> {
        self.ask(|done| Command::Snapshot(include_hosts, done))
            .await
    }

    /// The multiplexer's command queue, spawning the multiplexer first if
    /// it isn't running.
    fn commands(&self) -> &mpsc::UnboundedSender<Command> {
        self.inner.commands.get_or_init(|| {
            let (commands, received) = mpsc::unbounded_channel();
            tokio::spawn(run(self.inner.session.clone(), received));
            commands
        })
    }

    /// A handle that removes entries `ids` when dropped.
    fn handle(&self, ids: Vec<RuleId>) -> InterceptHandle {
        InterceptHandle {
            ids,
            commands: Some(self.commands().clone()),
        }
    }

    /// Send the command `make` builds around a reply sender, and await the
    /// reply.
    async fn ask<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, InterceptionError> {
        let (done, reply) = oneshot::channel();
        self.commands()
            .send(make(done))
            .map_err(|_| InterceptionError::SubscriptionClosed)?;
        reply
            .await
            .map_err(|_| InterceptionError::SubscriptionClosed)
    }
}

/// Keeps the entries of one `insert` (or one stream) in the list: dropping
/// the handle removes those still there, and [`stop`](Self::stop) removes
/// them and waits until Chrome has applied the change.
#[derive(Debug)]
#[must_use = "the entries are removed when the handle is dropped — bind it to a variable to keep them in"]
pub struct InterceptHandle {
    ids: Vec<RuleId>,
    /// `None` after `stop`, and in a handle from `for_tests`.
    commands: Option<mpsc::UnboundedSender<Command>>,
}

impl InterceptHandle {
    /// The entries' ids, in their order in the list when inserted.
    pub fn ids(&self) -> &[RuleId] {
        &self.ids
    }

    /// Test-support constructor: a handle for no entry, whose drop does
    /// nothing. For downstream unit tests that need an `InterceptHandle` to
    /// fill a registry, without a session. Gated behind the `test-support`
    /// feature so production builds don't expose it.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn for_tests() -> Self {
        Self {
            ids: Vec::new(),
            commands: None,
        }
    }

    /// Remove the entries still in the list, and wait until Chrome has
    /// applied the change.
    ///
    /// # Errors
    ///
    /// [`InterceptionError::Call`] if Chrome refused the change, or the
    /// session's socket couldn't carry it. The entries are removed either
    /// way. (If they were already gone, because they were removed or the
    /// session's socket closed, this returns `Ok`.)
    pub async fn stop(mut self) -> Result<(), InterceptionError> {
        let Some(commands) = self.commands.take() else {
            return Ok(());
        };
        let (done, applied) = oneshot::channel();
        let remove = Command::Remove {
            ids: std::mem::take(&mut self.ids),
            strict: false,
            done: Some(done),
        };
        if commands.send(remove).is_err() {
            return Ok(());
        }
        applied.await.unwrap_or(Ok(()))
    }
}

impl Drop for InterceptHandle {
    fn drop(&mut self) {
        if let Some(commands) = self.commands.take() {
            let remove = Command::Remove {
                ids: std::mem::take(&mut self.ids),
                strict: false,
                done: None,
            };
            let _ = commands.send(remove);
        }
    }
}

/// The paused requests that one stream entry decides, from
/// [`Interceptor::subscribe`].
///
/// Release each with one of [`PausedRequest`]'s methods. Dropping the
/// stream removes its entry, and continues each request it took that
/// wasn't read yet.
#[derive(Debug)]
#[must_use = "the entry is removed when the stream is dropped — bind it to a variable to keep it in"]
pub struct InterceptStream {
    requests: mpsc::UnboundedReceiver<PausedRequest>,
    handle: InterceptHandle,
}

impl InterceptStream {
    /// The stream entry's id.
    pub fn id(&self) -> RuleId {
        self.handle.ids[0]
    }
}

impl Stream for InterceptStream {
    type Item = PausedRequest;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<PausedRequest>> {
        self.get_mut().requests.poll_recv(cx)
    }
}

/// An entry of the list.
struct Slot {
    id: RuleId,
    label: Option<String>,
    when: Match,
    responder: Responder,
}

impl Slot {
    /// This entry as a snapshot shows it.
    fn entry(&self, include_hosts: bool) -> Entry {
        let url = match self.when.url_match() {
            UrlMatch::Glob(glob) => UrlInfo::Glob(glob.pattern_str().into()),
            UrlMatch::Hosts(hosts) => UrlInfo::Hosts {
                count: hosts.len(),
                hosts: include_hosts
                    .then(|| hosts.hosts().into_iter().map(str::to_owned).collect()),
            },
        };
        let action = match &self.responder {
            Responder::Stream(_) => ActionKind::Stream,
            Responder::Rule(Action::Continue) => ActionKind::Continue,
            Responder::Rule(Action::Block(reason)) => ActionKind::Block(*reason),
            Responder::Rule(Action::Respond { status, .. }) => {
                ActionKind::Respond { status: *status }
            }
            Responder::Rule(Action::RewriteUrl(to)) => ActionKind::RewriteUrl(to.clone()),
            Responder::Rule(Action::ModifyRequest(_)) => ActionKind::ModifyRequest,
            Responder::Rule(Action::ModifyResponse(_)) => ActionKind::ModifyResponse,
        };
        Entry {
            id: self.id,
            label: self.label.clone(),
            pause: self.when.pause(),
            url,
            resource_type: self.when.resource_type(),
            action,
        }
    }

    /// Whether this is a stream whose consumer dropped it (its removal is
    /// on its way): it decides nothing more.
    fn is_dropped_stream(&self) -> bool {
        matches!(&self.responder, Responder::Stream(requests) if requests.is_closed())
    }
}

/// What decides the requests an entry fits.
enum Responder {
    Rule(Action),
    /// The stream from `subscribe()`.
    Stream(mpsc::UnboundedSender<PausedRequest>),
}

/// The reply to a change.
type Reply = oneshot::Sender<Result<(), InterceptionError>>;

/// What an `Interceptor` or a handle asks of the multiplexer.
enum Command {
    /// Insert the entries at the position.
    Insert(Vec<Slot>, Position, Reply),
    /// Move the entry to the position.
    Move(RuleId, Position, Reply),
    /// Remove the entries still there; with `strict`, say it's an error
    /// if one wasn't.
    Remove {
        ids: Vec<RuleId>,
        strict: bool,
        done: Option<Reply>,
    },
    /// Replace the proxy credentials.
    SetProxyAuth(Option<ProxyAuth>, Reply),
    /// Describe the entries, with the hosts of host sets or not.
    Snapshot(bool, oneshot::Sender<Snapshot>),
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

/// The command (method and params) with which `action` answers the paused
/// request `ev`. `ev`'s pause kind is one `action` acts on (`Rule::new`
/// checked it, and the entry's match fit the pause).
fn command(action: &Action, ev: &RequestPausedEvent) -> (&'static str, Value) {
    let id = ev.request_id.as_str();
    let unchanged = || {
        let params = continue_params(id, RequestOverrides::default());
        ("Fetch.continueRequest", params)
    };
    match action {
        Action::Continue => unchanged(),
        Action::Block(reason) => ("Fetch.failRequest", fail_params(id, *reason)),
        Action::Respond {
            status,
            headers,
            body,
        } => (
            "Fetch.fulfillRequest",
            fulfill_params(id, *status, headers, body),
        ),
        Action::RewriteUrl(to) => {
            let overrides = RequestOverrides {
                url: Some(to.clone()),
                ..RequestOverrides::default()
            };
            ("Fetch.continueRequest", continue_params(id, overrides))
        }
        Action::ModifyRequest(modify) => {
            let info = build_request_info(ev);
            match run_closure(|| modify(&info)) {
                Some(overrides) => ("Fetch.continueRequest", continue_params(id, overrides)),
                None => unchanged(),
            }
        }
        Action::ModifyResponse(modify) => {
            // A ResponseHeaders pause always has a status.
            let Some(info) = build_response_info(ev) else {
                return unchanged();
            };
            match run_closure(|| modify(&info)) {
                Some(overrides) => (
                    "Fetch.continueResponse",
                    continue_response_params(id, &info, overrides),
                ),
                None => unchanged(),
            }
        }
    }
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

/// The `Fetch.RequestPattern` that pauses requests for `auth`: every
/// request, or every document, at the Request stage.
fn auth_pattern(auth: &ProxyAuth) -> Value {
    let mut pattern = json!({ "urlPattern": "*", "requestStage": "Request" });
    if auth.pause == AuthPause::DocumentsOnly {
        pattern["resourceType"] = json!(ResourceType::Document.as_cdp_str());
    }
    pattern
}

/// Make `session`'s `Fetch` configuration match `slots` and `auth`.
async fn configure(
    session: &SessionHandle,
    slots: &[Slot],
    auth: Option<&ProxyAuth>,
) -> Result<(), InterceptionError> {
    if slots.is_empty() && auth.is_none() {
        session.call("Fetch.disable", json!({})).await?;
        return Ok(());
    }
    let mut patterns: Vec<Value> = Vec::new();
    let wanted = slots.iter().map(|s| s.when.cdp_pattern());
    for pattern in wanted.chain(auth.map(auth_pattern)) {
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
    session
        .call(
            "Fetch.enable",
            json!({ "patterns": patterns, "handleAuthRequests": auth.is_some() }),
        )
        .await?;
    Ok(())
}

/// The multiplexer's state; see the module docs.
struct Mux {
    session: SessionHandle,
    /// The entries, in the order a paused request walks them.
    slots: Vec<Slot>,
    proxy_auth: Option<ProxyAuth>,
    /// The session's `Fetch` events, subscribed to while there are entries
    /// or credentials.
    events: Option<FetchEvents>,
    answered: Answered,
}

impl Mux {
    /// Carry out `command`, and reply to it.
    async fn obey(&mut self, command: Command) {
        match command {
            Command::Insert(slots, at, done) => {
                let _ = done.send(self.insert(slots, at).await);
            }
            Command::Move(id, at, done) => {
                let _ = done.send(self.move_slot(id, at));
            }
            Command::Remove { ids, strict, done } => {
                let missing = ids.iter().find(|id| self.position(**id).is_none()).copied();
                let before = self.slots.len();
                self.slots.retain(|s| !ids.contains(&s.id));
                let mut result = Ok(());
                if self.slots.len() != before {
                    result = self.apply().await;
                }
                if let (true, Some(id)) = (strict, missing) {
                    result = Err(InterceptionError::NoSuchRule(id));
                }
                match done {
                    Some(done) => {
                        let _ = done.send(result);
                    }
                    None => {
                        if let Err(e) = result {
                            debug!(error = %e, "interception: removing a dropped handle's entries failed");
                        }
                    }
                }
            }
            Command::SetProxyAuth(auth, done) => {
                let previous = std::mem::replace(&mut self.proxy_auth, auth);
                let applied = self.apply().await;
                if applied.is_err() {
                    self.proxy_auth = previous;
                    self.unsubscribe_if_idle();
                }
                let _ = done.send(applied);
            }
            Command::Snapshot(include_hosts, done) => {
                let snapshot = Snapshot {
                    entries: self.slots.iter().map(|s| s.entry(include_hosts)).collect(),
                    proxy_auth: self.proxy_auth.as_ref().map(|a| a.pause),
                };
                let _ = done.send(snapshot);
            }
        }
    }

    /// The index of entry `id`, if it's in the list.
    fn position(&self, id: RuleId) -> Option<usize> {
        self.slots.iter().position(|s| s.id == id)
    }

    /// The index at which `at` puts an entry.
    fn index(&self, at: Position) -> Result<usize, InterceptionError> {
        let of = |id| self.position(id).ok_or(InterceptionError::NoSuchRule(id));
        Ok(match at {
            Position::First => 0,
            Position::Last => self.slots.len(),
            Position::Before(id) => of(id)?,
            Position::After(id) => of(id)? + 1,
        })
    }

    /// Insert `slots` at `at`, and apply the change, or undo it if Chrome
    /// refused it.
    async fn insert(&mut self, slots: Vec<Slot>, at: Position) -> Result<(), InterceptionError> {
        let i = self.index(at)?;
        let ids: Vec<RuleId> = slots.iter().map(|s| s.id).collect();
        self.slots.splice(i..i, slots);
        let applied = self.apply().await;
        if applied.is_err() {
            self.slots.retain(|s| !ids.contains(&s.id));
            self.unsubscribe_if_idle();
        }
        applied
    }

    /// Move entry `id` to `at`. The patterns don't change, so Chrome isn't
    /// told.
    fn move_slot(&mut self, id: RuleId, at: Position) -> Result<(), InterceptionError> {
        let from = self.position(id).ok_or(InterceptionError::NoSuchRule(id))?;
        if matches!(at, Position::Before(a) | Position::After(a) if a == id) {
            return Ok(());
        }
        let slot = self.slots.remove(from);
        match self.index(at) {
            Ok(to) => {
                self.slots.insert(to, slot);
                Ok(())
            }
            Err(e) => {
                self.slots.insert(from, slot);
                Err(e)
            }
        }
    }

    /// Make Chrome's `Fetch` configuration match the entries and
    /// credentials, subscribed to the events before enabling `Fetch` (so no
    /// pause is missed) and unsubscribed once there's nothing to read them
    /// for.
    async fn apply(&mut self) -> Result<(), InterceptionError> {
        if self.events.is_none() && (!self.slots.is_empty() || self.proxy_auth.is_some()) {
            self.events = Some(fetch_events(&self.session));
        }
        let applied = configure(&self.session, &self.slots, self.proxy_auth.as_ref()).await;
        self.unsubscribe_if_idle();
        applied
    }

    /// Drop the event subscription if there are no entries or credentials.
    fn unsubscribe_if_idle(&mut self) {
        if self.slots.is_empty() && self.proxy_auth.is_none() {
            self.events = None;
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
        let applied = configure(&self.session, &self.slots, self.proxy_auth.as_ref()).await;
        if let Err(e) = applied {
            warn!(error = %e, "interception: Fetch.enable failed; the entries are inert");
        }
    }

    /// The session is gone (its socket closed, or it detached): drop every
    /// entry, which ends their streams.
    fn end(&mut self) {
        debug!("interception: the session is gone; dropping its entries");
        self.slots.clear();
        self.proxy_auth = None;
        self.events = None;
    }

    /// Hand the `Fetch.requestPaused` event `params` to the first entry that
    /// fits it (see the module docs).
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
        let pause = ev.pause_kind();
        let resource_type = parse_resource_type(ev.resource_type.as_deref());
        let url = &ev.request.url;
        let decider = self
            .slots
            .iter()
            .find(|s| s.when.fits(url, resource_type, pause) && !s.is_dropped_stream());
        let Some(decider) = decider else {
            let params = continue_params(&ev.request_id, RequestOverrides::default());
            release(
                &self.session,
                ev.request_id,
                "Fetch.continueRequest",
                params,
            );
            return;
        };
        match &decider.responder {
            Responder::Stream(requests) => {
                let paused = PausedRequest::new(
                    ev.request_id.clone(),
                    pause,
                    build_request_info(&ev),
                    build_response_info(&ev),
                    self.session.clone(),
                );
                // If the stream was dropped since, the request comes back,
                // and its drop continues it.
                let _ = requests.send(paused);
            }
            Responder::Rule(action) => {
                let (method, params) = command(action, &ev);
                release(&self.session, ev.request_id, method, params);
            }
        }
    }

    /// Answer the `Fetch.authRequired` event `params` with the session's
    /// credentials.
    fn authenticate(&mut self, params: &Value) {
        let Some(request_id) = params["requestId"].as_str() else {
            warn!("interception: Fetch.authRequired without requestId");
            return;
        };
        let credentials = self
            .proxy_auth
            .as_ref()
            .map(|a| (a.user.clone(), a.password.clone()));
        let response = auth_response(
            credentials.as_ref(),
            params,
            &mut self.answered,
            Instant::now(),
        );
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
        slots: Vec::new(),
        proxy_auth: None,
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

    /// What `add` (an `insert`, `subscribe`, `set_proxy_auth`, or a
    /// builder's `start`) returns, run while the mock answers its
    /// `Fetch.enable`; and that command's parameters.
    pub(crate) async fn applied<T>(
        mock: &mut MockConnection,
        add: impl Future<Output = Result<T, InterceptionError>>,
    ) -> (T, Value) {
        let (added, params) = futures::join!(add, answered(mock, "Fetch.enable"));
        (added.unwrap(), params)
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
    use crate::host_matcher::HostMatcher;
    use crate::types::ResponseOverrides;

    /// A rule for `glob` at the Request pause.
    fn rule(glob: &str, action: Action) -> Rule {
        Rule::new(Match::url(glob).unwrap(), action).unwrap()
    }

    /// The ids of `interceptor`'s entries, in order.
    async fn order(interceptor: &Interceptor) -> Vec<RuleId> {
        let snapshot = interceptor.snapshot(false).await.unwrap();
        snapshot.entries.iter().map(|e| e.id).collect()
    }

    /// The first entry that fits a pause decides it, and no later one runs:
    /// an `allow` ahead of a catch-all `respond` keeps it from the requests
    /// both fit.
    #[tokio::test]
    async fn the_first_fitting_entry_decides() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let rules = vec![
            rule("*/ads/*", Action::Block(AbortReason::BlockedByClient)),
            rule("*/keep/*", Action::Continue),
            rule(
                "*",
                Action::Respond {
                    status: 204,
                    headers: Vec::new(),
                    body: Vec::new(),
                },
            ),
        ];
        let (_handle, enable) = applied(&mut mock, interceptor.insert(rules, Position::Last)).await;
        assert_eq!(enable["handleAuthRequests"], false);

        pause(&mock, "R1", "https://x.test/ads/1", json!({})).await;
        assert_eq!(
            answered(&mut mock, "Fetch.failRequest").await,
            json!({ "requestId": "R1", "errorReason": "BlockedByClient" })
        );
        pause(&mock, "R2", "https://x.test/keep/1", json!({})).await;
        assert_eq!(
            answered(&mut mock, "Fetch.continueRequest").await,
            json!({ "requestId": "R2" })
        );
        pause(&mock, "R3", "https://x.test/other", json!({})).await;
        assert_eq!(
            answered(&mut mock, "Fetch.fulfillRequest").await["requestId"],
            "R3"
        );
        conn.shutdown();
    }

    /// `start` returns only after Chrome has replied to `Fetch.enable`.
    #[tokio::test]
    async fn insert_returns_once_chrome_has_applied_the_patterns() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let rules = vec![rule("*", Action::Block(AbortReason::Failed))];
        let mut insert = tokio::spawn({
            let interceptor = interceptor.clone();
            async move { interceptor.insert(rules, Position::Last).await }
        });
        let id = mock.expect_cmd("Fetch.enable").await;
        let early = tokio::time::timeout(Duration::from_millis(100), &mut insert).await;
        assert!(early.is_err(), "insert returned before Chrome replied");
        mock.reply(id, json!({})).await;
        let _handle = insert.await.unwrap().unwrap();
        conn.shutdown();
    }

    /// Positions put entries where they say, `move_rule` moves one without
    /// telling Chrome, and an id that names no entry is an error.
    #[tokio::test]
    async fn positions_place_entries_and_move_rule_moves_them() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let mut ids = Vec::new();
        let mut handles = Vec::new();
        for at in [Position::Last, Position::First] {
            let rules = vec![rule("*", Action::Continue)];
            let (handle, _) = applied(&mut mock, interceptor.insert(rules, at)).await;
            ids.push(handle.ids()[0]);
            handles.push(handle);
        }
        let [a, b] = [ids[0], ids[1]];
        for at in [Position::Before(a), Position::After(a)] {
            let rules = vec![rule("*", Action::Continue)];
            let (handle, _) = applied(&mut mock, interceptor.insert(rules, at)).await;
            ids.push(handle.ids()[0]);
            handles.push(handle);
        }
        let [c, d] = [ids[2], ids[3]];
        assert_eq!(order(&interceptor).await, [b, c, a, d]);

        interceptor.move_rule(d, Position::First).await.unwrap();
        interceptor.move_rule(b, Position::After(a)).await.unwrap();
        assert_eq!(order(&interceptor).await, [d, c, a, b]);

        let gone = RuleId(u64::MAX);
        let err = interceptor
            .move_rule(c, Position::Before(gone))
            .await
            .unwrap_err();
        assert!(matches!(err, InterceptionError::NoSuchRule(id) if id == gone));
        assert_eq!(order(&interceptor).await, [d, c, a, b]);
        let rules = vec![rule("*", Action::Continue)];
        let err = interceptor
            .insert(rules, Position::After(gone))
            .await
            .unwrap_err();
        assert!(matches!(err, InterceptionError::NoSuchRule(_)));
        assert_eq!(
            mock.try_recv_cmd(),
            None,
            "a move or a refused insert told Chrome"
        );
        conn.shutdown();
    }

    /// Chrome gets every entry's pattern, plus one for the credentials, and
    /// `handleAuthRequests` while there are credentials. Removing entries
    /// keeps the others; removing the last (with no credentials left)
    /// disables `Fetch`.
    #[tokio::test]
    async fn the_patterns_are_the_entries_and_the_credentials() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let rules = vec![rule("*/a/*", Action::Block(AbortReason::Failed))];
        let (first, _) = applied(&mut mock, interceptor.insert(rules, Position::Last)).await;
        let auth = ProxyAuth {
            user: "u".into(),
            password: "p".into(),
            pause: AuthPause::DocumentsOnly,
        };
        let ((), enable) = applied(&mut mock, interceptor.set_proxy_auth(Some(auth))).await;
        assert_eq!(
            enable,
            json!({
                "patterns": [
                    { "urlPattern": "*/a/*", "requestStage": "Request" },
                    { "urlPattern": "*", "requestStage": "Request", "resourceType": "Document" },
                ],
                "handleAuthRequests": true,
            })
        );

        let hosts = Arc::new(HostMatcher::new(["evil.test".to_string()]));
        let blocked = Rule::new(Match::hosts(hosts), Action::Block(AbortReason::Failed)).unwrap();
        let (second, _) = applied(
            &mut mock,
            interceptor.insert(vec![blocked], Position::First),
        )
        .await;
        drop(first);
        let enable = answered(&mut mock, "Fetch.enable").await;
        assert_eq!(
            enable["patterns"][0],
            json!({ "urlPattern": "*", "requestStage": "Request" })
        );

        let ((), enable) = applied(&mut mock, interceptor.set_proxy_auth(None)).await;
        assert_eq!(enable["handleAuthRequests"], false);
        drop(second);
        answered(&mut mock, "Fetch.disable").await;
        conn.shutdown();
    }

    /// An entry decides only its own kind of pause: a `modify_response` at
    /// ResponseHeaders never takes a failed response, which goes to the
    /// `respond` at ResponseError behind it.
    #[tokio::test]
    async fn an_entry_decides_only_its_kind_of_pause() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let headers = Match::url("*").unwrap().at(PauseKind::ResponseHeaders);
        let status = |_: &crate::ResponseInfo| ResponseOverrides {
            status: Some(203),
            ..ResponseOverrides::default()
        };
        let errors = Match::url("*").unwrap().at(PauseKind::ResponseError);
        let respond = Action::Respond {
            status: 503,
            headers: Vec::new(),
            body: Vec::new(),
        };
        let rules = vec![
            Rule::new(headers, Action::ModifyResponse(Arc::new(status))).unwrap(),
            Rule::new(errors, respond).unwrap(),
        ];
        let (_handle, enable) = applied(&mut mock, interceptor.insert(rules, Position::Last)).await;
        assert_eq!(
            enable["patterns"],
            json!([{ "urlPattern": "*", "requestStage": "Response" }])
        );

        let failed = json!({ "responseErrorReason": "ConnectionRefused" });
        pause(&mock, "R1", "https://x.test/", failed).await;
        let fulfilled = answered(&mut mock, "Fetch.fulfillRequest").await;
        assert_eq!(fulfilled["requestId"], "R1");
        assert_eq!(fulfilled["responseCode"], 503);
        pause(
            &mock,
            "R2",
            "https://x.test/",
            json!({ "responseStatusCode": 200 }),
        )
        .await;
        let continued = answered(&mut mock, "Fetch.continueResponse").await;
        assert_eq!(continued["responseCode"], 203);
        conn.shutdown();
    }

    /// A snapshot lists the entries in order, with their labels, matches and
    /// actions, and a host set's hosts only when asked for.
    #[tokio::test]
    async fn a_snapshot_describes_the_entries_in_order() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let hosts = Arc::new(HostMatcher::new([
            "b.test".to_string(),
            "a.test".to_string(),
        ]));
        let rules = vec![
            Rule::new(
                Match::hosts(hosts),
                Action::Block(AbortReason::BlockedByClient),
            )
            .unwrap()
            .label("trackers"),
            rule("*/x", Action::RewriteUrl("https://y.test/".into())),
        ];
        let (_handle, _) = applied(&mut mock, interceptor.insert(rules, Position::Last)).await;
        let snapshot = interceptor.snapshot(false).await.unwrap();
        assert_eq!(snapshot.entries[0].label.as_deref(), Some("trackers"));
        assert!(matches!(
            snapshot.entries[0].url,
            UrlInfo::Hosts {
                count: 2,
                hosts: None
            }
        ));
        assert_eq!(
            snapshot.entries[0].action,
            ActionKind::Block(AbortReason::BlockedByClient)
        );
        assert!(matches!(&snapshot.entries[1].url, UrlInfo::Glob(g) if g == "*/x"));
        assert_eq!(
            snapshot.entries[1].action,
            ActionKind::RewriteUrl("https://y.test/".into())
        );
        let with_hosts = interceptor.snapshot(true).await.unwrap();
        let UrlInfo::Hosts { hosts, .. } = &with_hosts.entries[0].url else {
            panic!("not a host set");
        };
        assert_eq!(
            hosts.as_deref(),
            Some(&["a.test".to_string(), "b.test".to_string()][..])
        );
        conn.shutdown();
    }

    /// A stream entry decides what it fits, ahead of the rules behind it. A
    /// request read and dropped unreleased is continued, and so is one
    /// never read when the stream is dropped.
    #[tokio::test]
    async fn a_stream_decides_what_it_fits_and_its_drop_continues_the_unread() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let rules = vec![rule("*", Action::Block(AbortReason::Failed))];
        let (_rules, _) = applied(&mut mock, interceptor.insert(rules, Position::Last)).await;
        let when = Match::url("*/s/*").unwrap();
        let (mut stream, _) =
            applied(&mut mock, interceptor.subscribe(when, Position::First)).await;

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
        assert_eq!(first.pause, PauseKind::Request);
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
        conn.shutdown();
    }

    /// A rule's closure that panics leaves the request continued, unchanged,
    /// and the multiplexer running.
    #[tokio::test]
    async fn a_panicking_closure_continues_the_request() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let panics = Action::ModifyRequest(Arc::new(|_| -> RequestOverrides {
            panic!("a bug in the closure")
        }));
        let rules = vec![rule("*", panics)];
        let (_handle, _) = applied(&mut mock, interceptor.insert(rules, Position::Last)).await;
        for id in ["R1", "R2"] {
            pause(&mock, id, "https://x.test/", json!({})).await;
            assert_eq!(
                answered(&mut mock, "Fetch.continueRequest").await,
                json!({ "requestId": id })
            );
        }
        conn.shutdown();
    }

    /// A release that Chrome refuses is followed by a plain
    /// `continueRequest`, and a pause whose event doesn't decode is
    /// continued by its `requestId`: neither request is left paused.
    #[tokio::test]
    async fn a_refused_or_unreadable_pause_is_continued() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let bad = Action::Respond {
            status: 200,
            headers: vec![("bad header".into(), "x".into())],
            body: Vec::new(),
        };
        let (_handle, _) = applied(
            &mut mock,
            interceptor.insert(vec![rule("*", bad)], Position::Last),
        )
        .await;

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

    /// A proxy's challenge gets the session's credentials; without any, it
    /// gets `Default`.
    #[tokio::test]
    async fn a_proxy_challenge_gets_the_sessions_credentials() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let challenge = |id: &str| {
            json!({
                "requestId": id,
                "authChallenge": { "source": "Proxy", "origin": "http://proxy.test", "scheme": "basic" },
            })
        };
        let rules = vec![rule("*/x", Action::Continue)];
        let (_rules, _) = applied(&mut mock, interceptor.insert(rules, Position::Last)).await;
        mock.emit_event_for_session("Fetch.authRequired", challenge("A1"), "S1")
            .await;
        let answer = answered(&mut mock, "Fetch.continueWithAuth").await;
        assert_eq!(answer["authChallengeResponse"]["response"], "Default");

        let auth = ProxyAuth {
            user: "u".into(),
            password: "p".into(),
            pause: AuthPause::EveryRequest,
        };
        applied(&mut mock, interceptor.set_proxy_auth(Some(auth))).await;
        mock.emit_event_for_session("Fetch.authRequired", challenge("A2"), "S1")
            .await;
        let answer = answered(&mut mock, "Fetch.continueWithAuth").await;
        assert_eq!(answer["requestId"], "A2");
        assert_eq!(
            answer["authChallengeResponse"]["response"],
            "ProvideCredentials"
        );
        assert_eq!(answer["authChallengeResponse"]["username"], "u");
        conn.shutdown();
    }

    /// Lost events make the multiplexer disable `Fetch` (which continues
    /// any pause it missed) and enable it again with every entry's pattern.
    #[tokio::test]
    async fn lost_events_disable_fetch_and_enable_it_again() {
        let (mut mock, conn) = MockConnection::pair_with_accounted_capacity(4);
        let interceptor = Interceptor::new(SessionHandle::new(conn.clone(), "S1"));
        let first = vec![rule("*/a/*", Action::Block(AbortReason::Failed))];
        let (_first, _) = applied(&mut mock, interceptor.insert(first, Position::Last)).await;

        // While the multiplexer waits for Chrome, more events arrive than
        // its bus holds.
        let second = vec![rule("*/b/*", Action::Block(AbortReason::Failed))];
        let second = tokio::spawn({
            let interceptor = interceptor.clone();
            async move { interceptor.insert(second, Position::Last).await }
        });
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
            json!([
                { "urlPattern": "*/a/*", "requestStage": "Request" },
                { "urlPattern": "*/b/*", "requestStage": "Request" },
            ])
        );
        conn.shutdown();
    }

    /// A stream ends when its session detaches from its target, or when the
    /// session's socket is shut.
    #[tokio::test]
    async fn a_stream_ends_with_its_session() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let when = || Match::url("*").unwrap();
        let (mut stream, _) =
            applied(&mut mock, interceptor.subscribe(when(), Position::Last)).await;
        mock.emit_event("Target.detachedFromTarget", json!({ "sessionId": "S1" }))
            .await;
        let next = tokio::time::timeout(Duration::from_secs(2), stream.next()).await;
        assert!(matches!(next, Ok(None)), "the stream didn't end on detach");

        let (mut stream, _) =
            applied(&mut mock, interceptor.subscribe(when(), Position::Last)).await;
        conn.shutdown();
        let next = tokio::time::timeout(Duration::from_secs(2), stream.next()).await;
        assert!(
            matches!(next, Ok(None)),
            "the stream didn't end on shutdown"
        );
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
