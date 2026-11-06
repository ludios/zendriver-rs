# Interception precedence: the Astra–Fable design (4 rounds, 2026-11-06)

Merged from both models' signed round-3 texts and their round-4 positions.
The user settled the two points they still disputed (see the end).

## Model

A tab's interception is **one ordered list of entries**. Each entry is one
rule or one stream. The **first** entry that fits a paused request decides it;
no later entry runs. Every insertion states its **position**; nothing is
ordered implicitly. Tracker blocking is an ordinary, visible entry. Proxy
credentials are one per-tab setting, outside the list.

Removed: layers, newest-first, the layer-level "considers" filter (`Filter`),
`acts_at` fall-through, `.pattern()` / `.at_request()` / `.at_response()` /
`.resource()`, `subscribe()` on the rules builder, per-layer credentials,
pass-on modifiers, numeric priorities, revisions.

## Rust

```rust
pub struct RuleId(u64);                                   // Copy, Eq, Hash, Display; never reused
pub enum Position  { First, Last, Before(RuleId), After(RuleId) }
pub enum PauseKind { Request, ResponseHeaders, ResponseError }
pub enum UrlMatch  { Glob(UrlPattern), Hosts(Arc<HostMatcher>) }
pub struct Match { pause: PauseKind, url: UrlMatch, resource_type: Option<ResourceType> }
impl Match {
    pub fn url(glob: &str) -> Result<Self>;               // Request, any resource type
    pub fn hosts(hosts: Arc<HostMatcher>) -> Self;        // Request, any resource type
    pub fn at(self, pause: PauseKind) -> Self;
    pub fn resource(self, t: ResourceType) -> Self;
}
pub enum Action {
    Continue,                                             // an explicit exception
    Block(AbortReason),
    Respond { status: u16, headers: Vec<(String, String)>, body: Vec<u8> },
    RewriteUrl(String),                                   // Request only
    ModifyRequest(Arc<dyn Fn(&RequestInfo) -> RequestOverrides + Send + Sync>),    // Request only
    ModifyResponse(Arc<dyn Fn(&ResponseInfo) -> ResponseOverrides + Send + Sync>), // ResponseHeaders only
}
pub struct Rule { label: Option<String>, when: Match, action: Action }
impl Rule { pub fn new(when: Match, action: Action) -> Result<Self>; pub fn label(self, l: &str) -> Self; }
pub struct Snapshot { entries: Vec<Entry>, proxy_auth: Option<AuthPause> }  // dispatch order
pub struct ProxyAuth { user: String, password: String, pause: AuthPause }
pub enum AuthPause { EveryRequest, DocumentsOnly }        // DocumentsOnly: a 407 on a parallel connection is missed

impl Interceptor {
    pub async fn insert(&self, rules: Vec<Rule>, at: Position) -> Result<InterceptHandle>;
    pub async fn subscribe(&self, when: Match, at: Position) -> Result<InterceptStream>;
    pub async fn move_rule(&self, id: RuleId, at: Position) -> Result<()>;
    pub async fn remove(&self, id: RuleId) -> Result<()>;            // any entry, browser.trackers included
    pub async fn set_proxy_auth(&self, auth: Option<ProxyAuth>) -> Result<()>;
    pub async fn snapshot(&self, include_hosts: bool) -> Result<Snapshot>;
}
impl InterceptHandle { pub fn ids(&self) -> &[RuleId]; pub async fn stop(self) -> Result<()>; } // drop removes too
impl InterceptStream { pub fn id(&self) -> RuleId; }      // Stream<Item = PausedRequest>
impl InterceptBuilder {                                   // sugar: each method pushes one Rule
    // allow, block, respond, redirect, modify_request(m: impl TryInto<Match>, ..)
    // modify_response (pause = ResponseHeaders), block_hosts(hosts)
    pub async fn start(self, at: Position) -> Result<InterceptHandle>;   // = insert(rules, at)
}
impl Tab {
    pub fn intercept(&self) -> InterceptBuilder;
    pub fn interceptor(&self) -> &Interceptor;
    pub fn tracker_blocking(&self) -> Option<RuleId>;
}
// BrowserBuilder::{block_trackers, tracker_blocklist_*, proxy_auth} and
// BrowserContextBuilder::proxy_auth are unchanged: they fill rules 2 and 9.
// (ProxyConfig / browser_open restructure: deferred, both agree.)
```

## Decision procedure (for the docs, verbatim)

1. A tab's interception is one list of entries, each one rule or one stream. `insert`, `start` and `subscribe` put the new entries where `Position` says in the list as it is at that moment; a builder's rules go in consecutively, in call order. `move_rule` and removal keep the other entries' order. `Before(id)` / `After(id)` naming no entry is an error.
2. A new tab's list holds one entry, `browser.trackers` (`Request`, `Block`, the hosts of `block_trackers` / `tracker_blocklist_*`), or none if those are off. It is ordinary: `First` goes ahead of it, `Last` behind it, `move_rule` moves it, `remove(tab.tracker_blocking())` deletes it.
3. Chrome pauses a request at the Request stage when some `Request` entry fits its URL and resource type, or while `ProxyAuth` is set (every request, or only documents under `DocumentsOnly`), and at the Response stage when some `ResponseHeaders` or `ResponseError` entry fits.
4. A pause's kind is `Request`, `ResponseHeaders` (Chrome has a status, 4xx and 5xx included) or `ResponseError` (the request failed before any status).
5. The first entry, front to back, whose `pause` is the pause's kind and whose URL and resource type fit it decides the pause. Its action answers Chrome, and no later entry runs. `Fetch`, `XHR` and `EventSource` are one resource type to Chrome. A pause no entry fits is continued unchanged.
6. Every pause walks the list anew: deciding a Request pause says nothing about the Response pause, and each 3xx hop is a new request to Chrome.
7. `RewriteUrl`, or a `ModifyRequest` returning a URL, sends the request to the new URL without pausing it again at the Request stage. It pauses at the Response stage exactly when some `ResponseHeaders` / `ResponseError` entry fits the new URL (sent as `interceptResponse`).
8. A stream entry owns every pause it decides. The consumer answers each through its `PausedRequest`, which hands the answer to the tab's interception to send (so rule 7 holds for it too); `continue_()` is the explicit pass.
9. A proxy's challenge gets the tab's `ProxyAuth` once; a repeated challenge is cancelled; a server's challenge always gets Chrome's default.
10. A pause whose deciding entry does not answer it fails the request (`Fetch.failRequest`, reason `Failed`), logged with the entry's id: a closure that panicked, an answer Chrome refused, a `PausedRequest` dropped unanswered, a stream dropped with pauses unread. No other entry is tried.

Keep `Fetch` enabled with `patterns: []` after the last entry goes while a stream still holds a pause (disabling would release it), and disable it once the last held pause is answered.

## MCP

```
browser_intercept_add_rule  { pattern | hosts: [..], resource_type?, pause?, label?,
                              action: { kind: continue|block|respond|redirect|modify_request|modify_response, .. },
                              position: "first" | "last" | { before: id } | { after: id } }   → { rule_id, rules }
browser_intercept_move_rule   { rule_id, position } → { rules }
browser_intercept_remove_rule { rule_id }           → { rules }   // any entry, browser.trackers included
browser_intercept_clear_rules {}                    → { rules }   // the entries the agent added; browser.trackers stays
browser_intercept_list_rules  { include_hosts? }    → { rules: [ { index, rule_id, origin: "browser"|"agent", label?, pause,
                                                         pattern? | hosts? | hosts_count, resource_type?, action } ],
                                                      proxy_auth: { configured, pause? } }
```

`position` has no default. Its description: "`last` unless this rule must take requests from an earlier entry. `first` is ahead of `browser.trackers`, so a `first` rule that fits a tracker's request decides it; `continue` or `modify_request` there sends the tracker request." `pause` defaults to `response_headers` for `modify_response`, else `request`. `rules` is `list_rules`'s output, read after the change.

## Examples

- `modify_request("*", f).start(Position::Last)` gives `[browser.trackers, modify *]`: trackers blocked, everything else modified. `First` reverses it, and both the call site and `list_rules` say the trackers get the header and are sent.
- MCP: `block */api/*` at `last`, then `respond */api/health` with `{before: A}` gives `[browser.trackers, respond, block]`. Both at `last` puts the respond at index 2, visibly shadowed, and health is blocked.
- `[browser.trackers, respond mock_api, stream *]`: the stream gets what the two did not decide. At `first` it gets everything, trackers included.
- `Rule::new(Match::url("*/api/*")?.at(ResponseError), Respond{503,..})` serves a synthetic reply when the network fails; a `modify_response` entry ahead of it never fits an error pause.

## Settled by the user (2026-11-06)

1. **`browser_intercept_clear_rules` removes only the entries the agent
   added** (Fable's position). `browser.trackers` stays, and the returned
   list shows it; an agent that wants it gone names it in `remove_rule`.
   (Astra had proposed removing every entry.)
2. **A stream consumer's answer goes through the multiplexer** (Astra's
   position), which sends it and owns its completion: that is what gives a
   stream's `modify_and_continue` with a new URL the `interceptResponse` of
   rule 7. (Fable had proposed sending it directly over the session.)
