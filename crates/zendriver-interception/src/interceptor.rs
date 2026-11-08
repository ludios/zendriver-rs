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
//! 6. A request sent to a new URL (an [`Action::RewriteUrl`], or a
//!    `ModifyRequest` or a stream's answer that returns a URL) isn't paused
//!    again at the Request stage. Its response is paused (sent as
//!    `interceptResponse`) when some `ResponseHeaders` / `ResponseError`
//!    entry is of its resource type, so that at that pause the entry that
//!    fits the new URL, as Chrome wrote it, decides (rule 4).
//! 7. A stream entry owns every pause it decides: the consumer answers each
//!    through its [`PausedRequest`], which hands the answer to the
//!    multiplexer to send. While a pause awaits its answer (a stream holds
//!    it, or Chrome hasn't replied to its answer yet), `Fetch` stays on
//!    (with no patterns, if no entry or credentials are left), since
//!    `Fetch.disable` would release it.
//! 8. A proxy's challenge gets the session's credentials once; a repeated
//!    challenge is cancelled; a server's challenge always gets Chrome's
//!    default.
//! 9. A pause whose deciding entry doesn't answer it fails the request
//!    (`Fetch.failRequest`, reason `Failed`), logged with the entry's id: a
//!    closure that panicked, an answer Chrome refused, a `PausedRequest`
//!    dropped unanswered, a stream dropped with pauses unread. No other
//!    entry is tried.
//!
//! If the multiplexer falls so far behind that the connection drops some
//! of its events, it disables `Fetch` and enables it again: a pause it
//! missed would otherwise stay paused, and `Fetch.disable` continues every
//! paused request, including those a stream holds (whose release then
//! fails). When the session's socket dies, is shut, or is replaced by a
//! reconnect, or the session detaches from its target, the entries are
//! dropped, which ends their streams.

use std::collections::{HashMap, HashSet, VecDeque};
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
use zendriver_transport::{CallError, RawEvent, SessionEnd, SessionHandle};

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
    /// missed, unless an entry pauses that request at the Request stage
    /// (Chrome surfaces the challenges of the requests it paused there).
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
/// spawned, until the first call of a method.
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

    /// Move entry `id` to `at` (a stream keeps the pauses it holds). A
    /// `Before(id)` / `After(id)` naming the entry itself leaves it where it
    /// is.
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
            tokio::spawn(run(
                self.inner.session.clone(),
                received,
                commands.downgrade(),
            ));
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
/// stream removes its entry, and fails each request it took that wasn't
/// read yet.
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
pub(crate) struct Slot {
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
pub(crate) enum Responder {
    Rule(Action),
    /// The stream from `subscribe()`.
    Stream(mpsc::UnboundedSender<PausedRequest>),
}

/// The reply to a change or an answer.
pub(crate) type Reply = oneshot::Sender<Result<(), InterceptionError>>;

/// A stream consumer's answer to a paused request, from its
/// [`PausedRequest`].
pub(crate) struct Answer {
    /// The pause's token (see [`Hold`]).
    pub(crate) hold: u64,
    pub(crate) request_id: String,
    pub(crate) method: &'static str,
    pub(crate) params: Value,
    /// Where Chrome's reply goes, if anyone waits for it.
    pub(crate) done: Option<Reply>,
}

/// A pause decided and not answered yet: a stream holds it, or Chrome
/// hasn't replied to its answer. Each has its own token, since a request
/// keeps its `requestId` from its Request pause to its Response pause.
struct Hold {
    request_id: String,
    /// The entry that decided it, or `None` if none did.
    entry: Option<RuleId>,
    /// The request's resource type, for the `interceptResponse` of an
    /// answer that sends it to a new URL.
    resource_type: ResourceType,
}

/// What an `Interceptor`, a handle or a paused request asks of the
/// multiplexer.
pub(crate) enum Command {
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
    /// Send a stream consumer's answer.
    Answer(Answer),
    /// A stream's paused request, of this hold token, was dropped
    /// unanswered.
    Dropped(u64),
    /// Chrome has replied to the answer of this hold token.
    Answered(u64),
}

/// A session's `Fetch` events, in order, until one `Err` says why they
/// stopped (see [`session_events`]).
type SessionEvents = Pin<Box<dyn Stream<Item = Result<Arc<RawEvent>, SessionEnd>> + Send>>;

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

/// `session`'s `Fetch` events ([`SessionHandle::events`]), in order, until
/// one `Err` says why they stopped (for example, the session detached).
/// The returned stream also ends at once, with no `Err`, when the socket is
/// shut or replaced (either cancels the socket's shutdown token), skipping
/// the events still queued, since no answer to them could be sent.
fn session_events(session: &SessionHandle) -> SessionEvents {
    let shut = session.connection().shutdown_token().cancelled_owned();
    Box::pin(session.events(&["Fetch."]).take_until(shut))
}

/// The next of `events`, or `None` if they ended. Never ready while there
/// are no events to read.
async fn next_event(
    events: &mut Option<SessionEvents>,
) -> Option<Result<Arc<RawEvent>, SessionEnd>> {
    match events {
        Some(events) => events.next().await,
        None => std::future::pending().await,
    }
}

/// The command (method and params) with which `action`, of entry `entry`,
/// answers the paused request `ev`. `ev`'s pause kind is one `action` acts
/// on (`Rule::new` checked it, and the entry's match fit the pause).
fn command(action: &Action, ev: &RequestPausedEvent, entry: RuleId) -> (&'static str, Value) {
    let id = ev.request_id.as_str();
    let unchanged = || {
        let params = continue_params(id, RequestOverrides::default());
        ("Fetch.continueRequest", params)
    };
    let failed = || ("Fetch.failRequest", fail_params(id, AbortReason::Failed));
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
            match run_closure(entry, || modify(&info)) {
                Some(overrides) => ("Fetch.continueRequest", continue_params(id, overrides)),
                None => failed(),
            }
        }
        Action::ModifyResponse(modify) => {
            // A ResponseHeaders pause always has a status.
            let Some(info) = build_response_info(ev) else {
                return unchanged();
            };
            match run_closure(entry, || modify(&info)) {
                Some(overrides) => (
                    "Fetch.continueResponse",
                    continue_response_params(id, &info, overrides),
                ),
                None => failed(),
            }
        }
    }
}

/// `f()`, the closure of entry `entry`, or `None` (logged) if it panicked.
/// A rule's closure is the caller's code, and a panic left to unwind would
/// end the multiplexer, leaving the session's requests paused for good.
fn run_closure<T>(entry: RuleId, f: impl FnOnce() -> T) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => Some(value),
        Err(_) => {
            error!(%entry, "interception: the closure of entry {entry} panicked; failing the request");
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

/// Release the paused request `request_id` with `method` and `params`, the
/// answer of entry `entry` (`None`: of no entry), and return Chrome's
/// reply. If Chrome refuses them, the request is failed instead (unless
/// the refused answer failed it): one left paused would stall its page for
/// good. After a timeout or a broken connection, Chrome may have taken the
/// answer, so nothing more is sent.
async fn answer_now(
    session: &SessionHandle,
    request_id: &str,
    entry: Option<RuleId>,
    method: &'static str,
    params: Value,
) -> Result<(), InterceptionError> {
    let Err(e) = session.call(method, params).await else {
        return Ok(());
    };
    let entry = entry.map(|e| e.0);
    if !matches!(e, CallError::Rpc(..)) || method == "Fetch.failRequest" {
        warn!(error = %e, method, entry, "interception: answering a paused request failed");
        return Err(e.into());
    }
    warn!(error = %e, method, entry, "interception: Chrome refused the answer to a paused request; failing the request");
    let failed = fail_params(request_id, AbortReason::Failed);
    if let Err(e) = session.call("Fetch.failRequest", failed).await {
        debug!(error = %e, "interception: failing the request failed too");
    }
    Err(e.into())
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

/// Make `session`'s `Fetch` configuration match `slots` and `auth`; with
/// `held_open`, keep `Fetch` on even with neither (a stream holds a pause,
/// which `Fetch.disable` would release).
async fn configure(
    session: &SessionHandle,
    slots: &[Slot],
    auth: Option<&ProxyAuth>,
    held_open: bool,
) -> Result<(), InterceptionError> {
    if slots.is_empty() && auth.is_none() && !held_open {
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
    /// The session's events, subscribed to while there are entries or
    /// credentials.
    events: Option<SessionEvents>,
    answered: Answered,
    /// The pauses decided and not answered yet, by token.
    held: HashMap<u64, Hold>,
    /// The last token given to a pause.
    last_hold: u64,
    /// A sender of the multiplexer's own queue, for paused requests'
    /// answers and their completions. Weak, so the multiplexer ends once
    /// nothing else can send it commands.
    commands: mpsc::WeakUnboundedSender<Command>,
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
            Command::Answer(Answer {
                hold,
                request_id,
                method,
                params,
                done,
            }) => self.send_answer(hold, request_id, method, params, done),
            Command::Dropped(hold) => {
                // Not held: `Fetch.disable` released it, or the session ended.
                let Some(Hold {
                    request_id, entry, ..
                }) = self.held.get(&hold)
                else {
                    return;
                };
                let entry = entry.map(|e| e.0);
                warn!(
                    entry,
                    "interception: a stream's paused request was dropped unanswered; failing it"
                );
                let params = fail_params(request_id, AbortReason::Failed);
                let request_id = request_id.clone();
                self.send_answer(hold, request_id, "Fetch.failRequest", params, None);
            }
            Command::Answered(hold) => {
                let released = self.held.remove(&hold).is_some();
                if released && self.is_idle() {
                    if let Err(e) = self.apply().await {
                        debug!(error = %e, "interception: Fetch.disable after the last held pause failed");
                    }
                }
            }
        }
    }

    /// Record a pause of `request_id`, of `resource_type`, decided by
    /// `entry` (`None`: by no entry), and return its token.
    fn hold(
        &mut self,
        request_id: &str,
        entry: Option<RuleId>,
        resource_type: ResourceType,
    ) -> u64 {
        self.last_hold += 1;
        let hold = Hold {
            request_id: request_id.into(),
            entry,
            resource_type,
        };
        self.held.insert(self.last_hold, hold);
        self.last_hold
    }

    /// Answer the pause of token `hold`, of request `request_id`, with
    /// `method` and `params` ([`answer_now`], with the `interceptResponse`
    /// of rule 6 if it sends the request to a new URL); reply to `done`, and
    /// tell the multiplexer once Chrome has replied.
    ///
    /// A pause no longer held was released by a `Fetch.disable` or the
    /// session's end: the answer is sent as it is, for its caller to hear
    /// Chrome refuse it.
    fn send_answer(
        &self,
        hold: u64,
        request_id: String,
        method: &'static str,
        params: Value,
        done: Option<Reply>,
    ) {
        let session = self.session.clone();
        let Some(Hold {
            entry,
            resource_type,
            ..
        }) = self.held.get(&hold)
        else {
            tokio::spawn(async move {
                let sent = session.call(method, params).await;
                if let Some(done) = done {
                    let _ = done.send(sent.map(|_| ()).map_err(Into::into));
                }
            });
            return;
        };
        let entry = *entry;
        let params = self.with_intercept_response(params, *resource_type);
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let sent = answer_now(&session, &request_id, entry, method, params).await;
            if let Some(done) = done {
                let _ = done.send(sent);
            }
            if let Some(commands) = commands.upgrade() {
                let _ = commands.send(Command::Answered(hold));
            }
        });
    }

    /// `params` of a `Fetch.continueRequest` that sends a request of
    /// `resource_type` to a new URL, with `interceptResponse` saying whether
    /// some response entry is of that type; other `params` unchanged.
    ///
    /// Chrome decided the Response pause from the old URL. Whether an entry
    /// fits the new one is left to that pause, which carries the URL as
    /// Chrome wrote it (`LOCALHOST` as `localhost`, say).
    fn with_intercept_response(&self, mut params: Value, resource_type: ResourceType) -> Value {
        if params.get("url").is_none() {
            return params;
        }
        let kinds = [PauseKind::ResponseHeaders, PauseKind::ResponseError];
        let wanted = self.slots.iter().any(|s| {
            !s.is_dropped_stream() && kinds.iter().any(|k| s.when.fits_any_url(resource_type, *k))
        });
        params["interceptResponse"] = Value::Bool(wanted);
        params
    }

    /// Whether `Fetch` needn't be on: there are no entries, no credentials
    /// and no held pauses (`Fetch.disable` would release them).
    fn is_idle(&self) -> bool {
        self.slots.is_empty() && self.proxy_auth.is_none() && self.held.is_empty()
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

    /// Make Chrome's `Fetch` configuration match the entries, credentials
    /// and held pauses, subscribed to the events before enabling `Fetch` (so
    /// no pause is missed), and unsubscribed once `Fetch.disable` succeeded.
    /// A change Chrome refused leaves the subscription for the caller's
    /// rollback to settle ([`unsubscribe_if_idle`](Self::unsubscribe_if_idle)).
    async fn apply(&mut self) -> Result<(), InterceptionError> {
        if self.events.is_none() && !self.is_idle() {
            self.events = Some(session_events(&self.session));
        }
        let held_open = !self.held.is_empty();
        let applied = configure(
            &self.session,
            &self.slots,
            self.proxy_auth.as_ref(),
            held_open,
        )
        .await;
        if applied.is_ok() {
            self.unsubscribe_if_idle();
        }
        applied
    }

    /// Drop the event subscription if `Fetch` needn't be on.
    fn unsubscribe_if_idle(&mut self) {
        if self.is_idle() {
            self.events = None;
        }
    }

    fn on_event(&mut self, event: &RawEvent) {
        match event.method.as_str() {
            "Fetch.requestPaused" => self.dispatch(&event.params),
            "Fetch.authRequired" => self.authenticate(&event.params),
            _ => {}
        }
    }

    /// The events overflowed their queue, so a request paused since may
    /// have been missed, and would stay paused for good: disable `Fetch`,
    /// which continues every request still paused (including those a
    /// stream holds), subscribe again, and enable `Fetch` again.
    async fn recover(&mut self) {
        warn!(
            "interception: its Fetch events overflowed their queue; disabling Fetch, which continues every paused request, then enabling it again"
        );
        self.events = None;
        match self.session.call("Fetch.disable", json!({})).await {
            // It released every pause.
            Ok(_) => self.held.clear(),
            Err(e) => {
                warn!(error = %e, "interception: Fetch.disable failed; the held pauses stay held")
            }
        }
        if let Err(e) = self.apply().await {
            warn!(error = %e, "interception: Fetch.enable failed; the entries are inert");
        }
    }

    /// Nothing can send the multiplexer commands any more (the last
    /// `Interceptor` clone, handle and paused request are gone), so nothing
    /// could change or answer the interception: turn `Fetch` off if it is
    /// on (the credentials outlive every handle).
    async fn close(&mut self) {
        if self.events.is_some() {
            if let Err(e) = self.session.call("Fetch.disable", json!({})).await {
                debug!(error = %e, "interception: Fetch.disable at the end failed");
            }
        }
    }

    /// The session is gone (its socket closed, or it detached): drop every
    /// entry, which ends their streams.
    fn end(&mut self) {
        debug!("interception: the session is gone; dropping its entries");
        self.slots.clear();
        self.proxy_auth = None;
        self.events = None;
        self.held.clear();
    }

    /// Hand the `Fetch.requestPaused` event `params` to the first entry that
    /// fits it (see the module docs).
    fn dispatch(&mut self, params: &Value) {
        let ev = match RequestPausedEvent::deserialize(params) {
            Ok(ev) => ev,
            Err(e) => {
                let Some(id) = params["requestId"].as_str() else {
                    warn!(error = %e, "interception: a Fetch.requestPaused event without requestId");
                    return;
                };
                warn!(error = %e, "interception: continuing a Fetch.requestPaused event it can't read");
                let hold = self.hold(id, None, ResourceType::Other);
                let unchanged = continue_params(id, RequestOverrides::default());
                self.send_answer(hold, id.into(), "Fetch.continueRequest", unchanged, None);
                return;
            }
        };
        let pause = ev.pause_kind();
        let resource_type = parse_resource_type(ev.resource_type.as_deref());
        let url = &ev.request.url;
        let decider = self
            .slots
            .iter()
            .position(|s| s.when.fits(url, resource_type, pause) && !s.is_dropped_stream());
        let Some(decider) = decider else {
            let hold = self.hold(&ev.request_id, None, resource_type);
            let unchanged = continue_params(&ev.request_id, RequestOverrides::default());
            self.send_answer(
                hold,
                ev.request_id,
                "Fetch.continueRequest",
                unchanged,
                None,
            );
            return;
        };
        let entry = self.slots[decider].id;
        let hold = self.hold(&ev.request_id, Some(entry), resource_type);
        match &self.slots[decider].responder {
            Responder::Stream(requests) => {
                // An `Interceptor` or handle that inserted the stream's entry
                // is alive while its stream is, unless the stream is being
                // dropped right now.
                let Some(answers) = self.commands.upgrade() else {
                    let failed = fail_params(&ev.request_id, AbortReason::Failed);
                    self.send_answer(hold, ev.request_id, "Fetch.failRequest", failed, None);
                    return;
                };
                let paused = PausedRequest::new(
                    ev.request_id.clone(),
                    pause,
                    build_request_info(&ev),
                    build_response_info(&ev),
                    self.session.clone(),
                    answers,
                    hold,
                );
                // If the stream was dropped since, the request comes back,
                // and its drop fails it.
                let _ = requests.send(paused);
            }
            Responder::Rule(action) => {
                let (method, params) = command(action, &ev, entry);
                self.send_answer(hold, ev.request_id, method, params, None);
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
async fn run(
    session: SessionHandle,
    mut commands: mpsc::UnboundedReceiver<Command>,
    own: mpsc::WeakUnboundedSender<Command>,
) {
    let mut mux = Mux {
        session,
        slots: Vec::new(),
        proxy_auth: None,
        events: None,
        answered: Answered::default(),
        held: HashMap::new(),
        last_hold: 0,
        commands: own,
    };
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    mux.close().await;
                    break;
                };
                mux.obey(command).await;
            }
            event = next_event(&mut mux.events) => match event {
                Some(Ok(event)) => mux.on_event(&event),
                Some(Err(SessionEnd::Overflowed)) => mux.recover().await,
                // Any other end: the session detached, or its socket died or
                // was shut or replaced (a new socket doesn't have it).
                Some(Err(_)) | None => mux.end(),
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
    /// request read and dropped unanswered is failed, and so is one never
    /// read when the stream is dropped: no rule behind it gets them.
    #[tokio::test]
    async fn a_stream_decides_what_it_fits_and_its_drop_fails_the_unread() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let rules = vec![rule("*", Action::Block(AbortReason::BlockedByClient))];
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
        // Pauses are dispatched in order, so once the rule has answered R3,
        // R2 is in the stream.
        pause(&mock, "R3", "https://x.test/other", json!({})).await;
        assert_eq!(
            answered(&mut mock, "Fetch.failRequest").await,
            json!({ "requestId": "R3", "errorReason": "BlockedByClient" })
        );
        let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.request.url, "https://x.test/s/1");
        assert_eq!(first.request.resource_type, ResourceType::XHR);
        assert_eq!(first.pause, PauseKind::Request);
        drop(first);
        drop(stream);
        let mut sent = sent_until_quiet(&mut mock).await;
        sent.retain(|(method, _)| method != "Fetch.enable");
        sent.sort_by_key(|(_, params)| params["requestId"].to_string());
        let failed = |id: &str| {
            let params = json!({ "requestId": id, "errorReason": "Failed" });
            ("Fetch.failRequest".to_string(), params)
        };
        assert_eq!(sent, vec![failed("R1"), failed("R2")]);
        conn.shutdown();
    }

    /// A request sent to a new URL pauses at the Response stage when a
    /// response entry is of its resource type, whether a rule or a stream
    /// sent it: there, the entry that fits the URL as Chrome wrote it
    /// decides.
    #[tokio::test]
    async fn a_new_url_pauses_at_the_response_stage_for_an_entry_of_its_type() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let api_responses = Match::url("*/api/*")
            .unwrap()
            .at(PauseKind::ResponseHeaders)
            .resource(ResourceType::XHR);
        let rules = vec![
            rule(
                "*/to-api",
                Action::RewriteUrl("https://B.TEST/api/1".into()),
            ),
            Rule::new(api_responses, Action::Continue).unwrap(),
        ];
        let (_rules, _) = applied(&mut mock, interceptor.insert(rules, Position::Last)).await;
        let when = Match::url("*/to-stream").unwrap();
        let (mut stream, _) = applied(&mut mock, interceptor.subscribe(when, Position::Last)).await;

        for (id, resource_type, intercept_response) in [("R1", "XHR", true), ("R2", "Image", false)]
        {
            let fields = json!({ "resourceType": resource_type });
            pause(&mock, id, "https://a.test/to-api", fields).await;
            let continued = answered(&mut mock, "Fetch.continueRequest").await;
            assert_eq!(continued["requestId"], id);
            assert_eq!(
                continued["interceptResponse"], intercept_response,
                "{resource_type}"
            );
        }

        let xhr = json!({ "resourceType": "XHR" });
        pause(&mock, "R3", "https://a.test/to-stream", xhr).await;
        let paused = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        let to_api = RequestOverrides {
            url: Some("https://b.test/api/2".into()),
            ..RequestOverrides::default()
        };
        let (sent, continued) = futures::join!(
            paused.modify_and_continue(to_api),
            answered(&mut mock, "Fetch.continueRequest")
        );
        sent.unwrap();
        assert_eq!(
            continued,
            json!({ "requestId": "R3", "url": "https://b.test/api/2", "interceptResponse": true })
        );
        conn.shutdown();
    }

    /// While a stream holds a pause, removing the last entry leaves `Fetch`
    /// on with no patterns (`Fetch.disable` would release the pause), and
    /// pauses Chrome sent meanwhile are still answered; the answer to the
    /// held pause turns `Fetch` off.
    #[tokio::test]
    async fn fetch_stays_on_while_a_stream_holds_a_pause() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let when = Match::url("*").unwrap();
        let (mut stream, _) = applied(&mut mock, interceptor.subscribe(when, Position::Last)).await;
        pause(&mock, "R1", "https://x.test/", json!({})).await;
        let held = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();

        drop(stream);
        let enabled = (
            "Fetch.enable".to_string(),
            json!({ "patterns": [], "handleAuthRequests": false }),
        );
        assert_eq!(sent_until_quiet(&mut mock).await, vec![enabled]);
        pause(&mock, "R2", "https://x.test/", json!({})).await;
        assert_eq!(
            answered(&mut mock, "Fetch.continueRequest").await,
            json!({ "requestId": "R2" })
        );

        let (sent, continued) = futures::join!(
            held.continue_(),
            answered(&mut mock, "Fetch.continueRequest")
        );
        sent.unwrap();
        assert_eq!(continued, json!({ "requestId": "R1" }));
        let disabled = ("Fetch.disable".to_string(), json!({}));
        assert_eq!(sent_until_quiet(&mut mock).await, vec![disabled]);
        conn.shutdown();
    }

    /// A request keeps its `requestId` from its Request pause to its
    /// Response pause, so each pause is held on its own: Chrome's late reply
    /// to the Request pause's answer leaves the Response pause held, and its
    /// drop fails it.
    #[tokio::test]
    async fn each_pause_of_a_request_is_held_on_its_own() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let at_request = Match::url("*").unwrap();
        let (mut requests, _) =
            applied(&mut mock, interceptor.subscribe(at_request, Position::Last)).await;
        let at_response = Match::url("*").unwrap().at(PauseKind::ResponseHeaders);
        let (mut responses, _) = applied(
            &mut mock,
            interceptor.subscribe(at_response, Position::Last),
        )
        .await;
        async fn next(stream: &mut InterceptStream) -> PausedRequest {
            let next = tokio::time::timeout(Duration::from_secs(2), stream.next());
            next.await.unwrap().unwrap()
        }

        pause(&mock, "R1", "https://x.test/", json!({})).await;
        let continued = tokio::spawn(next(&mut requests).await.continue_());
        let id = mock.expect_cmd("Fetch.continueRequest").await;
        let response = json!({ "responseStatusCode": 200, "responseHeaders": [] });
        pause(&mock, "R1", "https://x.test/", response).await;
        let at_response = next(&mut responses).await;
        mock.reply(id, json!({})).await;
        continued.await.unwrap().unwrap();

        drop(at_response);
        assert_eq!(
            answered(&mut mock, "Fetch.failRequest").await,
            json!({ "requestId": "R1", "errorReason": "Failed" })
        );
        conn.shutdown();
    }

    /// Removing a rule while Chrome hasn't replied to its answer leaves
    /// `Fetch` on until Chrome has (`Fetch.disable` would release the
    /// request unanswered).
    #[tokio::test]
    async fn fetch_stays_on_until_an_answer_is_in() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let rules = vec![rule("*", Action::Block(AbortReason::Failed))];
        let (handle, _) = applied(&mut mock, interceptor.insert(rules, Position::Last)).await;
        pause(&mock, "R1", "https://x.test/", json!({})).await;
        let id = mock.expect_cmd("Fetch.failRequest").await;

        let (stopped, sent) = futures::join!(handle.stop(), sent_until_quiet(&mut mock));
        stopped.unwrap();
        let enabled = (
            "Fetch.enable".to_string(),
            json!({ "patterns": [], "handleAuthRequests": false }),
        );
        assert_eq!(sent, vec![enabled]);
        mock.reply(id, json!({})).await;
        let disabled = ("Fetch.disable".to_string(), json!({}));
        assert_eq!(sent_until_quiet(&mut mock).await, vec![disabled]);
        conn.shutdown();
    }

    /// A change Chrome refuses leaves the interception as it was, still
    /// answering: here, the credentials stay, and so does the subscription
    /// to the challenges they answer.
    #[tokio::test]
    async fn a_refused_change_leaves_the_credentials_answering() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let auth = ProxyAuth {
            user: "u".into(),
            password: "p".into(),
            pause: AuthPause::EveryRequest,
        };
        applied(&mut mock, interceptor.set_proxy_auth(Some(auth))).await;
        let unset = tokio::spawn({
            let interceptor = interceptor.clone();
            async move { interceptor.set_proxy_auth(None).await }
        });
        let id = mock.expect_cmd("Fetch.disable").await;
        mock.reply_err(id, -32000, "refused").await;
        assert!(unset.await.unwrap().is_err());

        let challenge = json!({
            "requestId": "A1",
            "authChallenge": { "source": "Proxy", "origin": "http://proxy.test", "scheme": "basic" },
        });
        mock.emit_event_for_session("Fetch.authRequired", challenge, "S1")
            .await;
        let answer = answered(&mut mock, "Fetch.continueWithAuth").await;
        assert_eq!(answer["authChallengeResponse"]["username"], "u");
        conn.shutdown();
    }

    /// Proxy credentials outlive every handle, so the multiplexer turns
    /// `Fetch` off when the last `Interceptor` clone goes: nothing would
    /// answer the requests Chrome pauses.
    #[tokio::test]
    async fn dropping_the_last_interceptor_turns_fetch_off() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let auth = ProxyAuth {
            user: "u".into(),
            password: "p".into(),
            pause: AuthPause::EveryRequest,
        };
        applied(&mut mock, interceptor.set_proxy_auth(Some(auth))).await;
        drop(interceptor);
        assert_eq!(answered(&mut mock, "Fetch.disable").await, json!({}));
        conn.shutdown();
    }

    /// A `failRequest` Chrome refuses isn't sent again.
    #[tokio::test]
    async fn a_refused_fail_is_not_sent_again() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let rules = vec![rule("*", Action::Block(AbortReason::Failed))];
        let (_handle, _) = applied(&mut mock, interceptor.insert(rules, Position::Last)).await;
        pause(&mock, "R1", "https://x.test/", json!({})).await;
        let id = mock.expect_cmd("Fetch.failRequest").await;
        mock.reply_err(id, -32602, "Invalid InterceptionId.").await;
        assert_eq!(sent_until_quiet(&mut mock).await, Vec::new());
        conn.shutdown();
    }

    /// A rule's closure that panics leaves the request failed (no entry
    /// behind it is tried), and the multiplexer running.
    #[tokio::test]
    async fn a_panicking_closure_fails_the_request() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let panics = Action::ModifyRequest(Arc::new(|_| -> RequestOverrides {
            panic!("a bug in the closure")
        }));
        let rules = vec![rule("*", panics), rule("*", Action::Continue)];
        let (_handle, _) = applied(&mut mock, interceptor.insert(rules, Position::Last)).await;
        for id in ["R1", "R2"] {
            pause(&mock, id, "https://x.test/", json!({})).await;
            let failed = (
                "Fetch.failRequest".to_string(),
                json!({ "requestId": id, "errorReason": "Failed" }),
            );
            assert_eq!(sent_until_quiet(&mut mock).await, vec![failed]);
        }
        conn.shutdown();
    }

    /// An answer that Chrome refuses is followed by a `failRequest`, and a
    /// pause whose event doesn't decode (no entry decided it) is continued
    /// by its `requestId`: neither request is left paused.
    #[tokio::test]
    async fn a_refused_answer_fails_and_an_unreadable_pause_continues() {
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
            answered(&mut mock, "Fetch.failRequest").await,
            json!({ "requestId": "R1", "errorReason": "Failed" })
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

    /// An overflow of its events' queue makes the multiplexer disable
    /// `Fetch` (which continues any pause it missed) and enable it again
    /// with every entry's pattern.
    #[tokio::test]
    async fn an_overflow_disables_fetch_and_enables_it_again() {
        let (mut mock, conn) = MockConnection::pair_with_session_queue_capacity(4);
        let interceptor = Interceptor::new(SessionHandle::new(conn.clone(), "S1"));
        let first = vec![rule("*/a/*", Action::Block(AbortReason::Failed))];
        let (_first, _) = applied(&mut mock, interceptor.insert(first, Position::Last)).await;

        // While the multiplexer waits for Chrome, more events arrive than
        // its queue holds (of a method it ignores, so the only commands it
        // sends are the recovery's).
        let second = vec![rule("*/b/*", Action::Block(AbortReason::Failed))];
        let second = tokio::spawn({
            let interceptor = interceptor.clone();
            async move { interceptor.insert(second, Position::Last).await }
        });
        let id = mock.expect_cmd("Fetch.enable").await;
        for _ in 0..10 {
            mock.emit_event_for_session("Fetch.unknown", json!({}), "S1")
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

    /// A recovery whose `Fetch.disable` Chrome refuses has released nothing,
    /// so the pauses held stay held: a stream's request dropped afterwards
    /// is still failed.
    #[tokio::test]
    async fn a_refused_recovery_keeps_the_held_pauses() {
        let (mut mock, conn) = MockConnection::pair_with_session_queue_capacity(4);
        let interceptor = Interceptor::new(SessionHandle::new(conn.clone(), "S1"));
        let (mut stream, _) = applied(
            &mut mock,
            interceptor.subscribe(Match::url("*").unwrap(), Position::Last),
        )
        .await;
        pause(&mock, "R1", "https://x.test/", json!({})).await;
        let held = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();

        // While the multiplexer waits for Chrome, more events arrive than
        // its queue holds (of a method it ignores, so the only commands it
        // sends are the recovery's).
        let rules = vec![rule("*/b/*", Action::Block(AbortReason::Failed))];
        let insert = tokio::spawn({
            let interceptor = interceptor.clone();
            async move { interceptor.insert(rules, Position::Last).await }
        });
        let id = mock.expect_cmd("Fetch.enable").await;
        for _ in 0..10 {
            mock.emit_event_for_session("Fetch.unknown", json!({}), "S1")
                .await;
        }
        mock.reply(id, json!({})).await;
        let _rules = insert.await.unwrap().unwrap();
        let id = mock.expect_cmd("Fetch.disable").await;
        mock.reply_err(id, -32000, "refused").await;
        answered(&mut mock, "Fetch.enable").await;

        drop(held);
        assert_eq!(
            answered(&mut mock, "Fetch.failRequest").await,
            json!({ "requestId": "R1", "errorReason": "Failed" })
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
