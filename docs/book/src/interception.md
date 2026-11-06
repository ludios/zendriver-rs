# Interception

The `interception` Cargo feature wraps Chrome's [`Fetch`] CDP domain: it
lets you block, redirect, synthesize, or rewrite any request a page makes —
useful for ad blocking, response mocking, header injection, and
offline-replay tests.

[`Fetch`]: https://chromedevtools.github.io/devtools-protocol/tot/Fetch/

Enable it in `Cargo.toml`:

```toml
[dependencies]
zendriver = { version = "0.1", features = ["interception"] }
```

## One ordered list per tab

A tab's interception is **one ordered list of entries**, each one rule or
one stream, and the **first entry that fits a paused request decides it**.
Every insertion says where in the list it goes: `Position::First`,
`Position::Last`, `Position::Before(id)` or `Position::After(id)`. Nothing
is ordered behind your back, and [`snapshot`] shows the list as a paused
request walks it. In full (from [`zendriver::interception`]):

1. `insert`, `start`, `subscribe` and `move_rule` put entries where their
   `Position` says in the list as it is at that moment; a builder's rules go
   in consecutively, in call order. Removing an entry keeps the others'
   order. A `Before(id)` / `After(id)` naming no entry is an error.
2. Chrome pauses a request at the Request stage when some `Request` entry
   fits its URL and resource type, or while proxy credentials are set (every
   request, or only documents under `AuthPause::DocumentsOnly`), and at the
   Response stage when some `ResponseHeaders` or `ResponseError` entry fits.
3. A pause's kind is `Request`, `ResponseHeaders` (Chrome has a status, 4xx
   and 5xx included) or `ResponseError` (the request failed before any
   status).
4. The first entry, front to back, whose `Match` fits the pause (its kind,
   URL and resource type) decides it: its action answers Chrome, and no
   later entry runs. A pause no entry fits is continued unchanged.
5. Every pause walks the list anew: deciding a Request pause says nothing
   about the Response pause, and each 3xx hop is a new request to Chrome.
6. A stream entry owns every pause it decides: the consumer answers each
   through its `PausedRequest`.
7. A proxy's challenge gets the tab's credentials once; a repeated
   challenge is cancelled; a server's challenge always gets Chrome's
   default.

With [tracker blocking](#tracker--fingerprinter-blocklist) on, a tab's list
starts with the entry `browser.trackers`. A rule at `Last` comes after it,
so tracker requests stay blocked whatever the rule does; a rule at `First`
decides the tracker requests it fits ahead of it, so `allow` or
`modify_request` there lets them through, visibly so in the call and in
[`snapshot`].

[`snapshot`]: https://docs.rs/zendriver/latest/zendriver/interception/struct.Interceptor.html#method.snapshot
[`zendriver::interception`]: https://docs.rs/zendriver/latest/zendriver/interception/interceptor/index.html

## Rules

[`Tab::intercept`] returns an [`InterceptBuilder`], shorthand for
inserting rules: each method adds one rule, and [`start`] inserts them at
the `Position` it is given and returns once Chrome intercepts, so a request
sent afterwards is decided by them. Each method's `glob` is matched against
the request's URL in CDP wildcard syntax, with Chrome's meaning: `*` matches
any characters, `?` matches zero or one character, and `\` makes the next
character literal (`\*` matches a `*`).

| Method | Action | Use case |
|--------|--------|----------|
| [`block`] | `Fetch.failRequest` with `BlockedByClient` | Ad / tracker blocking |
| `block_hosts` | the same, for a set of hosts and their subdomains | Large blocklists |
| `allow` | `Fetch.continueRequest`, unchanged | An exception ahead of a broader entry |
| [`redirect`] | `Fetch.continueRequest` with a new URL | Move an endpoint without page changes |
| [`respond`] | `Fetch.fulfillRequest` with a synthetic body | Mock an API for tests |
| [`modify_request`] | `Fetch.continueRequest` with overrides | Inject headers, change method/body |
| [`modify_response`] | `Fetch.continueResponse` with overrides, at the `ResponseHeaders` pause | Change a response's status or headers |

For anything else — another kind of pause, a resource type, a label — build
a `Rule` from a `Match` and an `Action` and add it with `.rule(..)`, or
`insert` it through [`Tab::interceptor`]:

```rust,ignore
use zendriver::interception::{AbortReason, Action, Match, PauseKind, Position, ResourceType, Rule};

// Serve a stand-in when the API can't be reached at all.
let offline = Match::url("*/api/*")?.at(PauseKind::ResponseError);
let stand_in = Action::Respond { status: 503, headers: vec![], body: b"offline".to_vec() };
// Block third-party scripts only.
let scripts = Match::url("https://cdn.example.com/*")?.resource(ResourceType::Script);
let _rules = tab
    .interceptor()
    .insert(
        vec![
            Rule::new(offline, stand_in)?.label("api offline"),
            Rule::new(scripts, Action::Block(AbortReason::BlockedByClient))?,
        ],
        Position::Last,
    )
    .await?;
```

[`Tab::intercept`]: https://docs.rs/zendriver/latest/zendriver/struct.Tab.html#method.intercept
[`Tab::interceptor`]: https://docs.rs/zendriver/latest/zendriver/struct.Tab.html#method.interceptor
[`InterceptBuilder`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html
[`start`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html#method.start
[`block`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html#method.block
[`redirect`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html#method.redirect
[`respond`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html#method.respond
[`modify_request`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html#method.modify_request
[`modify_response`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html#method.modify_response

### Blocking ads

```rust,no_run
{{#include ../../../crates/zendriver/examples/intercept_block_ads.rs}}
```

[`start`] returns an [`InterceptHandle`]. The handle keeps its rules in the
list — when you drop it, they are removed, and the tab's other entries
keep their order (`Fetch.disable` is sent once no entry and no credentials
are left). Await [`stop`] instead of dropping it to wait until Chrome has
applied the change. **Bind the handle to a variable; letting it drop
immediately silently removes its rules.** Use `let _intercept = ...` (note
the leading underscore: a plain `_` is no binding at all, so the handle
drops at the end of the statement, not of the scope).

[`InterceptHandle`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptHandle.html
[`stop`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptHandle.html#method.stop
[`RequestInfo`]: https://docs.rs/zendriver/latest/zendriver/struct.RequestInfo.html

### Modifying headers

CDP semantics for `headers` on `Fetch.continueRequest` is **replacement**,
not merge — every header you want sent must appear in the returned map.
The `modify_request` closure receives a [`RequestInfo`] with the
original headers; copy them forward before stamping your additions on
top:

```rust,no_run
{{#include ../../../crates/zendriver/examples/intercept_modify_headers.rs}}
```

The headers you copy forward are in a stable order, but not the browser's
original one: CDP delivers request headers as an object keyed by header name,
so Chrome has already merged duplicate names and the on-the-wire ordering is
gone before the closure runs. Response headers (`modify_response`) keep both,
since CDP sends those as an array.

The closure runs synchronously per-event on the tab's interception task;
it should not block, and can't wait for anything async. When deciding the
override needs an async call, use a [stream](#streams) instead.

### Exceptions and order

Since the first fitting entry decides, an exception goes ahead of the
broader entry it excepts:

```rust,ignore
use zendriver::interception::Position;

let block_api = tab.intercept().block("*/api/*")?.start(Position::Last).await?;
let _health = tab
    .intercept()
    .respond("*/api/health", 200, vec![], b"ok".to_vec())?
    .start(Position::Before(block_api.ids()[0]))
    .await?;
// /api/health gets the stand-in; every other /api/ request is blocked.
```

Two rules of one `start` go in in call order, so within one builder the
earlier rule wins where both fit. `move_rule` moves an entry later, and
`remove` takes out any entry, `browser.trackers` included.

## Streams

When rules are too restrictive — e.g. you need to inspect the upstream
response body before deciding what to do, or you want to forward events
to your own pipeline — [`subscribe`] inserts a stream entry instead:

```rust,ignore
use futures::StreamExt;
use zendriver::interception::{Match, PauseKind, Position};
use zendriver::AbortReason;

let responses = Match::url("*")?.at(PauseKind::ResponseHeaders);
let mut stream = tab.interceptor().subscribe(responses, Position::Last).await?;

while let Some(paused) = stream.next().await {
    let body = paused.body().await?;
    if body.windows(7).any(|w| w == b"BLOCKED") {
        paused.abort(AbortReason::BlockedByClient).await?;
    } else {
        paused.continue_().await?;
    }
}
```

Each [`PausedRequest`] is consumed by exactly one of
[`continue_`] / [`abort`] / [`respond`][paused-respond] / [`modify_and_continue`] /
[`continue_response`] (at a `ResponseHeaders` pause). The [`body`] method
is `&self` and reads the upstream response body non-destructively — only
useful at a `ResponseHeaders` pause; before the response Chrome has no body.

[`subscribe`]: https://docs.rs/zendriver/latest/zendriver/interception/struct.Interceptor.html#method.subscribe
[`PausedRequest`]: https://docs.rs/zendriver/latest/zendriver/struct.PausedRequest.html
[`continue_`]: https://docs.rs/zendriver/latest/zendriver/struct.PausedRequest.html#method.continue_
[`abort`]: https://docs.rs/zendriver/latest/zendriver/struct.PausedRequest.html#method.abort
[paused-respond]: https://docs.rs/zendriver/latest/zendriver/struct.PausedRequest.html#method.respond
[`modify_and_continue`]: https://docs.rs/zendriver/latest/zendriver/struct.PausedRequest.html#method.modify_and_continue
[`continue_response`]: https://docs.rs/zendriver/latest/zendriver/struct.PausedRequest.html#method.continue_response
[`body`]: https://docs.rs/zendriver/latest/zendriver/struct.PausedRequest.html#method.body

**Release every `PausedRequest` promptly.** Chrome holds the request
until one of the terminal methods arrives, so a request your consumer
holds on to stalls the page. A `PausedRequest` dropped without one that
Chrome accepted (say, after a panic mid-flow, or an action whose headers
Chrome refused) is continued unchanged, and so is each request the stream
took that nobody read when the stream is dropped. Rules get the same
treatment: a rule whose closure panics, or whose release Chrome refuses,
logs it and continues the request unchanged. And if the tab's interception
ever falls so far behind that it loses events (a pause it never saw would
stay paused), it disables and re-enables `Fetch`, which continues every
paused request, those a stream holds included. When the tab's socket
closes or its session detaches, its entries are dropped, which ends their
streams.

## Proxy credentials

A tab's proxy credentials are one setting, outside the list:
[`set_proxy_auth`] replaces them (`None` removes them). `BrowserBuilder::proxy_auth`
sets them on every tab of the default browser context, and
`BrowserContextBuilder::proxy_auth` on every tab of that context. They go
only to a proxy's challenge, never to a site's (HTTP 401). Chrome surfaces a
challenge only for a request it paused at the Request stage, so while
credentials are set it pauses every request (`AuthPause::EveryRequest`), or
only documents (`AuthPause::DocumentsOnly`: fewer round trips, but a 407 on
another request's connection is missed unless an entry pauses it).

[`set_proxy_auth`]: https://docs.rs/zendriver/latest/zendriver/interception/struct.Interceptor.html#method.set_proxy_auth

## Tracker / fingerprinter blocklist

The `tracker-blocking` feature builds a ready-made host-blocking rule on top
of interception, so you can block third-party trackers and fingerprinters
without writing patterns. Enable it on the builder and every tab starts with
it, as the first entry of its list, `browser.trackers` (see
[One ordered list per tab](#one-ordered-list-per-tab)); its id is
[`Tab::tracker_blocking`].

```toml
[dependencies]
zendriver = { version = "0.1", features = ["tracker-blocking"] }
```

```rust,no_run
use zendriver::Browser;

let browser = Browser::builder()
    .block_trackers(true)                        // curated bundled list
    .tracker_blocklist_add(["ads.example.com"])  // + your own hosts
    .launch().await?;
```

| Method | Effect |
|--------|--------|
| [`block_trackers(true)`] | Enable blocking with the curated bundled list (`include_str!`-embedded — only adds binary size when the feature is on) |
| [`tracker_blocklist_add`] | Add extra hosts (repeatable; implicitly enables blocking) |
| [`tracker_blocklist_file`] | Add hosts from a local file (one host per line; `0.0.0.0 host` hosts-file lines tolerated) |
| [`tracker_blocklist_url`] | Add hosts fetched from a URL at launch (cached on disk after the first fetch) |

Matching is host-based: a blocked host also blocks its subdomains. The bundle
ships only our own clean list — point `tracker_blocklist_url` at a third-party
list only if you accept that list's license.

The URL fetch runs inside `launch()` and is bounded: 5s to connect, 20s for the
whole request, 32 MiB of body. None of those are configurable, and a source that
exceeds one fails the launch. For a mirror that is slow or unusually large,
pre-seed the cache or point `tracker_blocklist_file` at a local copy.

Those three bounds are the only things about a URL source that can fail a
launch. The body is decoded as UTF-8 lossily, so a list whose licence header
carries a latin-1 byte still loads: host lines are ASCII, and the replacement
character only ever lands in text the parser discards.

[`Tab::tracker_blocking`]: https://docs.rs/zendriver/latest/zendriver/struct.Tab.html#method.tracker_blocking
[`block_trackers(true)`]: https://docs.rs/zendriver/latest/zendriver/struct.BrowserBuilder.html#method.block_trackers
[`tracker_blocklist_add`]: https://docs.rs/zendriver/latest/zendriver/struct.BrowserBuilder.html#method.tracker_blocklist_add
[`tracker_blocklist_file`]: https://docs.rs/zendriver/latest/zendriver/struct.BrowserBuilder.html#method.tracker_blocklist_file
[`tracker_blocklist_url`]: https://docs.rs/zendriver/latest/zendriver/struct.BrowserBuilder.html#method.tracker_blocklist_url

## Gotchas

- **`Fetch.enable` serializes the network.** Chrome routes every matched
  request through the JSON-RPC channel — round-trip per request adds
  latency. On heavy pages, expect 10-30% throughput loss even with a
  no-op `continue_` handler. Scope patterns tightly; prefer
  resource-type filters over `*` patterns.
- **A stream decides everything it fits.** A `subscribe()` of `*` at
  `First` gets every request ahead of every other entry, tracker requests
  included: block or continue each one yourself, or put it at `Last`.
- **CSP-restricted pages.** `Fetch.fulfillRequest` synthesizes responses
  that may violate a page's `Content-Security-Policy` (especially for
  scripts). Combine with `StealthProfile::spoofed()` which sets
  `bypass_csp = true` if you're synthesizing scripts.
- **HTTPS responses can't be re-signed.** `respond` and `modify_request`
  work for the wire payload Chrome sees, not for upstream-TLS-signed
  artifacts.

## See also

- [`InterceptBuilder` rustdoc] for every shorthand method, and the
  [`zendriver::interception`] module for `Interceptor`, `Rule`, `Match`,
  `Action` and `Position`.
- The [design record](https://github.com/TurtIeSocks/zendriver-rs/blob/main/docs/api-audit/interception-precedence.md)
  of the ordered list.
- [`Expect()`](./expect.md) for the orthogonal "wait for this network
  event" surface — `expect_request` observes without holding traffic;
  `intercept` actively rewrites it.

[`InterceptBuilder` rustdoc]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html
