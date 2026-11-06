# Interception

The `interception` Cargo feature wraps Chrome's [`Fetch`] CDP domain in a
fluent rule-based API plus a lower-level `Stream` of paused requests. It
lets you block, redirect, synthesize, or rewrite any subresource a page
asks for — useful for ad blocking, response mocking, header injection,
and offline-replay tests.

[`Fetch`]: https://chromedevtools.github.io/devtools-protocol/tot/Fetch/

Enable it in `Cargo.toml`:

```toml
[dependencies]
zendriver = { version = "0.1", features = ["interception"] }
```

The entry point is [`Tab::intercept`], which returns an
[`InterceptBuilder`] for one layer of that tab's interception. The builder
has two terminal methods, both `async`: [`start`] adds a layer of
declarative rules, and [`subscribe`] adds a layer that hands each paused
request to the returned stream, for the manual escape-hatch path. Both
return once Chrome intercepts, so a request sent afterwards is
intercepted.

[`Tab::intercept`]: https://docs.rs/zendriver/latest/zendriver/struct.Tab.html#method.intercept
[`InterceptBuilder`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html
[`start`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html#method.start
[`subscribe`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html#method.subscribe

## Rule-based API

The rule methods chain on the builder and dispatch the matching
terminal action automatically when the URL pattern fires. Patterns use
CDP wildcard syntax, with Chrome's meaning: `*` matches any characters, `?`
matches zero or one character, and `\` makes the next character literal
(`\*` matches a `*`).

| Method | Action | Use case |
|--------|--------|----------|
| [`block`] | `Fetch.failRequest` with `BlockedByClient` | Ad / tracker blocking |
| [`redirect`] | `Fetch.continueRequest` with new URL | Move an endpoint without page changes |
| [`respond`] | `Fetch.fulfillRequest` with synthetic body | Mock an API for tests |
| [`modify_request`] | `Fetch.continueRequest` with overrides | Inject headers, change method/body |
| [`modify_response`] | `Fetch.continueResponse` with overrides, once the response is in | Change a response's status or headers |

[`block`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html#method.block
[`redirect`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html#method.redirect
[`respond`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html#method.respond
[`modify_request`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html#method.modify_request
[`modify_response`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html#method.modify_response

### Blocking ads

```rust,no_run
{{#include ../../../crates/zendriver/examples/intercept_block_ads.rs}}
```

[`start`] returns an [`InterceptHandle`]. The handle keeps the rules in
force — when you drop it, its layer is removed, and the tab's other layers
keep working (`Fetch.disable` is sent once no layer is left). Await
[`stop`] instead of dropping it to wait until Chrome has applied the
change. **Bind the handle to a variable; letting it drop immediately
silently disables interception.**
Use `let _intercept = ...` (note the leading underscore — Rust would warn
on a plain `_`, which is a different binding semantics that drops at end
of statement, not end of scope).

[`stop`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptHandle.html#method.stop
[`InterceptHandle`]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptHandle.html
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
it should not block. Spawn off the runtime if you need to call out to an async
service before deciding the override.

### Redirect and synthesize

```rust,ignore
let _intercept = tab.intercept()
    .redirect("*/old-api/*", "https://example.com/new-api/")?
    .respond(
        "*/api/health*",
        200,
        vec![("content-type".into(), "application/json".into())],
        b"{\"ok\":true}".to_vec(),
    )?
    .start()
    .await?;
```

Both `redirect` and `respond` reach an internal `Fetch.continueRequest`
or `Fetch.fulfillRequest` — the first rule that matches decides per event,
so a single `InterceptHandle` can host any mix of the rule types.

### Several layers on one tab

Each `start()` or `subscribe()` adds a layer, and a tab's layers share its
one `Fetch` configuration: Chrome pauses what any of them asks for. A
paused request goes to the layers newest first. A layer considers it only
if one of its own patterns matches the request's URL, resource type and
stage; a rule layer takes it if one of its rules matches, a stream layer
takes every request it considers, and a request no layer takes is
continued unchanged. So a later layer overrides an earlier one where both
apply, and leaves it working everywhere else. The browser's own
[tracker blocking](#tracker--fingerprinter-blocklist) and proxy
credentials are the tab's oldest layer. A proxy's challenge gets the
credentials of the newest layer that has some.

## Stream API

When rules are too restrictive — e.g. you need to inspect the upstream
response body before deciding what to do, or you want to forward events
to your own pipeline — call [`subscribe`] instead of `start`:

```rust,ignore
use futures::StreamExt;
use zendriver::{AbortReason, RequestStage};

let mut stream = tab
    .intercept()
    .at_response()  // pause AFTER headers come back
    .subscribe()
    .await?;

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
[`continue_`] / [`abort`] / [`respond`] / [`modify_and_continue`]. The
[`body`] method is `&self` and reads the upstream response body
non-destructively — only useful at the `Response` stage; at the `Request`
stage Chrome has no body yet.

[`PausedRequest`]: https://docs.rs/zendriver/latest/zendriver/struct.PausedRequest.html
[`continue_`]: https://docs.rs/zendriver/latest/zendriver/struct.PausedRequest.html#method.continue_
[`abort`]: https://docs.rs/zendriver/latest/zendriver/struct.PausedRequest.html#method.abort
[`respond`]: https://docs.rs/zendriver/latest/zendriver/struct.PausedRequest.html#method.respond
[`modify_and_continue`]: https://docs.rs/zendriver/latest/zendriver/struct.PausedRequest.html#method.modify_and_continue
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
paused request, those a stream holds included.

## Tracker / fingerprinter blocklist

The `tracker-blocking` feature builds a ready-made host-blocking rule on top
of interception, so you can block third-party trackers and fingerprinters
without writing patterns. Enable it on the builder and it installs on the main
tab and every new tab automatically.

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

[`block_trackers(true)`]: https://docs.rs/zendriver/latest/zendriver/struct.BrowserBuilder.html#method.block_trackers
[`tracker_blocklist_add`]: https://docs.rs/zendriver/latest/zendriver/struct.BrowserBuilder.html#method.tracker_blocklist_add
[`tracker_blocklist_file`]: https://docs.rs/zendriver/latest/zendriver/struct.BrowserBuilder.html#method.tracker_blocklist_file
[`tracker_blocklist_url`]: https://docs.rs/zendriver/latest/zendriver/struct.BrowserBuilder.html#method.tracker_blocklist_url

## Pattern + stage filters

By default a rule layer pauses only what its rules act on: each rule's URL
pattern, at the stage the rule acts at (`Response` for `modify_response`,
`Request` for the others); `block_hosts` and proxy credentials pause every
request at the `Request` stage. A `subscribe()` stream pauses every request
at the `Request` stage. Choose the patterns yourself with builder
modifiers:

```rust,ignore
use zendriver::ResourceType;

let _h = tab.intercept()
    .pattern("*/static/*")   // URL filter for next-emitted RequestPattern
    .resource(ResourceType::Image)  // only images
    .at_response()           // pause after response headers, not before request
    .block("*/static/*")?
    .start()
    .await?;
```

Each call to `pattern()` opens a new `Fetch.RequestPattern`; chained
`resource` / `at_request` / `at_response` modifiers mutate that most-recent
pattern. The patterns are also the requests the layer considers (see
[Several layers on one tab](#several-layers-on-one-tab)). Within a layer,
the first rule that matches the URL and can act at the stage wins: a
`modify_response` rule is skipped before there is a response, and
`redirect` / `modify_request` after, so a later rule can take the
request.

## Gotchas

- **`Fetch.enable` serializes the network.** Chrome routes every matched
  request through the JSON-RPC channel — round-trip per request adds
  latency. On heavy pages, expect 10-30% throughput loss even with a
  no-op `continue_` handler. Scope patterns tightly; prefer
  resource-type filters over `*` patterns.
- **A stream layer takes everything it pauses.** A `subscribe()` with the
  default `*` pattern sees every request before any older layer does,
  tracker blocking included: block or continue each one yourself.
- **CSP-restricted pages.** `Fetch.fulfillRequest` synthesizes responses
  that may violate a page's `Content-Security-Policy` (especially for
  scripts). Combine with `StealthProfile::spoofed()` which sets
  `bypass_csp = true` if you're synthesizing scripts.
- **HTTPS responses can't be re-signed.** `respond` and `modify_request`
  work for the wire payload Chrome sees, not for upstream-TLS-signed
  artifacts.

## See also

- [`InterceptBuilder` rustdoc] for every modifier method.
- [`Expect()`](./expect.md) for the orthogonal "wait for this network
  event" surface — `expect_request` observes without holding traffic;
  `intercept` actively rewrites it.

[`InterceptBuilder` rustdoc]: https://docs.rs/zendriver/latest/zendriver/struct.InterceptBuilder.html
