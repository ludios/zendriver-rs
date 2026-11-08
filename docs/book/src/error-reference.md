# Error Reference

Every fallible API in zendriver returns
[`Result<T>`](https://docs.rs/zendriver/latest/zendriver/type.Result.html),
an alias for `std::result::Result<T, ZendriverError>`. This page lists
every public variant of [`ZendriverError`] plus the sub-crate errors
that flow into it via `#[from]`, with the common cause + the fix.

[`ZendriverError`]: https://docs.rs/zendriver/latest/zendriver/enum.ZendriverError.html

The enum is `#[non_exhaustive]` — pattern matches should include a `_`
arm. Per [SEMVER](https://github.com/TurtIeSocks/zendriver-rs/blob/main/SEMVER.md),
new variants may land in minor releases.

## Top-level `ZendriverError` variants

| Variant | Common cause | Fix |
|---------|-------------|-----|
| `Browser(BrowserError)` | Chrome launch / discovery failed. | See [BrowserError](#browsererror-variants) below. |
| `Transport(TransportError)` | WebSocket failure (Chrome crashed, socket reset); or `SessionDetached`: one tab's CDP session is gone (its tab closed) while the connection lives on. | For `SessionDetached`, drop that tab's handles and go on. Otherwise retry the operation; if recurring, check Chrome's stderr for crash dumps. |
| `Disconnected` | The connection to Chrome dropped without a caller-requested shutdown of it: Chrome crashed or quit, or the socket was severed. A tab that has its own socket also gets this during and after `browser.close()`, because Chrome quits. | If the browser wasn't closed: reconnect via `Browser::reconnect`, then re-acquire any `Tab`/`Frame`/`Element` handles (a reconnect invalidates them) via `Browser::tabs` (`Browser::main_tab` still returns the old tab). |
| `Cdp { code, message, data }` | Chrome returned a CDP RPC error. | Inspect `message` — `Invalid params` usually means a stale `RemoteObjectId` or wrong type signature. |
| `ElementNotFound { selector }` | Query selector matched zero elements within the timeout. | Confirm the page actually rendered the element (`tab.wait_for_load()` / `wait_for_idle()`); check the selector with `tab.find().css(...).count()`. |
| `ConflictingSelectors` | A query mixed a single-selector method (`.css`/`.xpath`/`.text`/`.role`) with bs4-like predicate methods (`.tag`/`.attr*`/`.containing_text`/…). | Use one selector style per query — either a single selector *or* combinable predicates, not both. |
| `Timeout(Duration)` | Generic operation timeout. | Increase the timeout via the builder, or address the underlying slow operation. |
| `CdpTimeout { method, budget }` | One CDP call ran out its per-call budget — either Chrome accepted it and never answered, or it never left the send queue. Unlike `Timeout(Duration)` it names the stuck method. | Raise the budget with `Connection::set_call_timeout` (whole connection) or `call_raw_with_timeout` (one call); a role query's `Accessibility.queryAXTree` keeps its own 10 s budget (see "Activating a tab" in [Multi-tab](./multi-tab.md#activating-a-tab)). A repeat offender on the same method usually means a wedged renderer — restart the browser. |
| `EventStreamIncomplete` | A wait that must see every event missed some, because it fell so far behind its events that their queue overflowed (4,096 unread): `tab.wait_for_idle_opts(IdleOptions { loss_policy: IdleLossPolicy::Strict, .. })`, the `expect_*` expectations, `tab.expect_navigation()`, or `tab.wait_for_url()`. The first two also report this way the end of the tab's events (the socket dying or being replaced by `Browser::reconnect`, a shutdown, or the tab's closing); `expect_navigation` and `wait_for_url` report that as `Navigation`. Also `MatchedDownload::wait` / `save_to`, when zendriver may have missed how the download ended (its state is then `DownloadProgressState::Unknown`). | For the idle wait, re-issue it (after `Browser::reconnect` first if the connection itself dropped), or switch to `IdleLossPolicy::Lenient` (the default) to tolerate gaps. For `expect_navigation`, the navigation may already have happened: check the destination with `tab.wait_for_url(...)`, which asks the page where it is, instead of arming a new expectation. For a download, Chrome may have finished it anyway: look in `MatchedDownload::download_dir()`. |
| `Navigation(String)` | Page navigation failed (DNS, refused, crashed), **or** an in-flight call lost its context to a navigation, **or** the tab closed, or its connection ended (the socket died, or was shut down), while `wait_for_load`, `expect_navigation`, or `wait_for_url` waited. | Check the URL / DNS / network; for the context-lost case, sequence the action after `tab.wait_for_load()`; for the connection's end, reconnect first unless you closed the tab or browser yourself. |
| `JsException(String)` | A JS expression in `evaluate()` raised an exception, **or** a query's JavaScript threw, which it does when the query's CSS selector, XPath or regex is invalid (the query fails with the browser's `SyntaxError` at once, instead of waiting out its timeout). | For `evaluate()`, wrap the JS in `(() => { try { return ...; } catch (e) { return null; } })()` if you want a soft failure; otherwise fix the JS. For a query, fix the selector or pattern: a regex is JavaScript syntax, so for case-insensitivity write `(?i:…)` (Chrome 125 or newer) rather than Rust's `(?i)`, or pass `"i"` to `text_regex_with_flags`. |
| `ElementStale` | An element's CDP handle invalidated **and** the auto-refresh path failed. | Re-issue the original query manually. |
| `NotRefreshable` | An element returned from raw `evaluate()` went stale and can't be replayed (no underlying selector). | Use `tab.find()` instead of `evaluate` when you need an element you'll hold across DOM mutations. |
| `NotActionable(Duration, reason)` | Element wasn't visible/enabled/stable/hit-testable within the gate timeout. | See [FAQ entry](./faq.md#why-am-i-getting-notactionable). Use `click_fast` to skip the gate when intentional. |
| `FrameNotFound(String)` | `tab.frame_by_url/name/id(...)` matched no frame. | Confirm the iframe is loaded; `tab.frames().await?` to inspect what frames Chrome sees. |
| `TabNotFound(String)` | A lookup of a tab by its id failed (zendriver-mcp's tab tools return it). | List the live tabs (`browser_tab_list`, or `browser.tabs()`) and use one of their ids. |
| `TabSetup { target_id, reason }` | A tab (from `new_tab()` or its siblings, or the main tab from `launch()` / `connect()`) wasn't set up. Either its target was detached first (an observer failed on it, and so the transport detached it, or the page closed), or the setup didn't end within its budget (5 s for each observer in the chain, plus 5 s), as for a page that zendriver doesn't set up as a tab, such as `chrome://version`. | Read `reason`. For a detach, look in the log for an observer that failed. |
| `Cookie(String)` | Cookie operation refused by Chrome (malformed domain, mixed origin, etc.). | Read the message — most often a domain / path mismatch. |
| `Storage(String)` | DOM storage operation refused (origin mismatch). | Confirm `tab.url()` matches the storage origin before the call. |
| `HistoryNavigation(String)` | `back()` / `forward()` with no entry to go to. | Check `tab.history_length().await?` first. |
| `Serde(serde_json::Error)` | JSON serialization at the CDP boundary failed. | Almost always indicates a type-mismatch bug in zendriver — please file an issue with the failing call. |
| `Io(std::io::Error)` | File I/O failure (screenshot write, upload read). | Standard `std::io::Error` handling; check permissions. |
| `Stealth(StealthError)` | Fingerprint resolution failed at launch. | See [StealthError](#stealtherror-variants) below. |
| `Interception(InterceptionError)` | Interception-layer error (gated `interception`). | See [InterceptionError](#interceptionerror-variants) below. |
| `Cloudflare(CloudflareError)` | Cloudflare bypass error (gated `cloudflare`). | See [CloudflareError](#cloudflareerror-variants) below. |
| `Fetcher(FetcherError)` | Chrome download error (gated `fetcher`). | See [FetcherError](#fetchererror-variants) below. |

## `BrowserError` variants

Sub-error returned wrapped in `ZendriverError::Browser`.

| Variant | Common cause | Fix |
|---------|-------------|-----|
| `ExecutableNotFound { searched }` | No Chrome on `$PATH` or in conventional install locations. | Install Chrome / Chromium, or pass `.executable(path)` to the builder. Use the `fetcher` feature to auto-download. |
| `SpawnFailed(io::Error)` | OS refused to spawn the binary (permissions, missing libs). | Check the wrapped `io::Error`; `Permission denied` → chmod, `No such file` → bad path. |
| `EarlyExit(ExitStatus)` | Chrome exited before printing `DevTools listening on`. Typical: `user_data_dir` locked by another Chrome, missing GPU sandbox on Linux. | Free the user-data-dir (kill stale Chrome processes), or pass `.arg("--no-sandbox")` if running in a container. |
| `WsTimeout` | Launch: Chrome printed nothing within the WS-endpoint wait window. `connect`: the `/json/version` reply of an `http://` endpoint took too long. | Launch: try headed mode (`headless(false)`) to see Chrome's window — usually reveals a missing dependency. `connect`: check that the endpoint is Chrome's DevTools port. |
| `GpuBackendUnavailable` | A launch that requested [`GpuBackend::Native`](https://docs.rs/zendriver/latest/zendriver/enum.GpuBackend.html) could not get a hardware GPU: either Chrome never advertised its WS endpoint (no GPU present, a crashed GPU process, a missing GPU sandbox), or Chrome started and the launch-time check found its WebGL pipeline was not hardware-accelerated. | See [GPU backend](./gpu-backend.md#no-automatic-fallback). Catch this error and retry with `GpuBackend::SwiftShader` or `GpuBackend::Disabled`; zendriver does not fall back automatically. |
| `DevtoolsParse` | Launch: Chrome's stderr announced a DevTools URL that didn't parse. | Should not happen with stable Chrome; file a bug. |
| `ConnectEndpoint { endpoint, reason }` | `connect`: the endpoint isn't a `ws://`, `wss://` or `http://` URL with a host (`https://` is rejected: the DevTools server has no TLS), connecting to it failed, or its `/json/version` reply wasn't a `200` with a `webSocketDebuggerUrl`. `reason` says which, with the reply's status line and the start of its body. | Check the endpoint, e.g. `curl http://HOST:PORT/json/version`. |
| `Cleanup(io::Error)` | `tempfile` cleanup of the `user_data_dir` failed. | Usually harmless; check filesystem permissions if persistent. |

## `TransportError` variants

Re-exported from `zendriver-transport`. Surfaced via
`ZendriverError::Transport`.

| Variant | Common cause | Fix |
|---------|-------------|-----|
| `Disconnected` | Chrome closed the WebSocket without a Close frame. Typically Chrome crashed. | Restart the browser; check syslog / dmesg for OOM kills. |
| `Ws(tungstenite::Error)` | Underlying WebSocket error. | Inspect the wrapped error — `ConnectionClosed` is benign during shutdown. |
| `Frame(serde_json::Error)` | JSON framing failed on a CDP message. | Indicates a Chrome protocol drift; file an issue. |
| `Shutdown` | Actor was told to shut down; pending calls drain with this. | Expected during graceful `browser.close()`; only an error if it surprises you. |
| `SessionDetached` | Chrome detached the session the command was sent on (its tab closed, or it was detached), with the command in flight or before it was sent. The connection and the other sessions are unaffected. | Drop that tab's `Tab` / `Frame` / `Element` handles. Expected if you closed the tab yourself. |
| `ResponseDropped { id }` | Actor replied but the caller's `oneshot` receiver had dropped. | Should not happen in normal use; indicates a panic somewhere up-stack. |
| `Io(io::Error)` | I/O error inside tungstenite. | Standard I/O handling. |

`CallError` is the transport's per-call result type; it folds into
`ZendriverError` automatically via the `From` impl — you won't see it
directly in the public surface.

## `StealthError` variants

Sub-error returned wrapped in `ZendriverError::Stealth`.

| Variant | Common cause | Fix |
|---------|-------------|-----|
| `PatchFailed { patch, source }` | A specific stealth patch CDP call failed. | Read the source `CallError`; usually means the target page navigated mid-patch. Retry the launch. |
| `ChromeVersionDetect(String)` | The Chrome version probe failed — reading the binary's PE version resource on Windows, or running `chrome --version` on Unix. Non-fatal on its own: the probe falls back to a baked-in version. | Confirm the binary path; pass `.chrome_version(N)` to the stealth profile to skip the probe. |
| `SystemInfo(String)` | `sysinfo` couldn't read RAM / CPU count. | Pass `.memory_gb(N).cpu_count(N)` overrides to skip the auto-detect. |
| `InvalidOverride(String)` | A fingerprint override value was outside the validated range (e.g. `memory_gb = 0`). | Read the message; fix the override. |

## `InterceptionError` variants

Gated `interception`. Sub-error returned wrapped in
`ZendriverError::Interception`.

| Variant | Common cause | Fix |
|---------|-------------|-----|
| `Call(CallError)` | Underlying CDP call failed: for `start()` / `insert()` / `subscribe()` / `remove()` / `stop()`, Chrome refused the patterns or the tab is gone; for a `PausedRequest` action, Chrome refused the answer (the request is then failed). | Inspect inner error. |
| `NoSuchRule(RuleId)` | `move_rule` / `remove`, or a `Position::Before` / `After`, named an entry that isn't in the list (never inserted, or removed since). | Read the ids from `snapshot()`. |
| `NoRules` | `insert` / `start` got no rules. | Add a rule first. |
| `MismatchedPause { action, pause }` | `Rule::new` got an action that can't act on the match's kind of pause (e.g. `ModifyResponse` on a `Request` pause). | `RewriteUrl` / `ModifyRequest` need `Request`, `ModifyResponse` needs `ResponseHeaders`. |
| `InvalidPattern(String)` | URL pattern didn't parse as CDP wildcard syntax. | Patterns use `*` / `?` (not regex). Escape a literal `*`, `?` or `\` with `\`. |
| `SubscriptionClosed` | The tab's interception task is gone (it panicked, or the runtime shut down), so a change couldn't be made, or a `PausedRequest`'s answer couldn't be sent. | Should not happen; file a bug. |
| `WrongStage` | `PausedRequest::continue_response` on a request paused other than at a `ResponseHeaders` pause (a `Request` or `ResponseError` pause; the request, dropped unanswered, is then failed). | Subscribe with a `Match` at `PauseKind::ResponseHeaders` (`Match::url(..)?.at(PauseKind::ResponseHeaders)`). |
| `InvalidResponse(String)` | A CDP response didn't carry the expected field (e.g. `Fetch.getResponseBody` returned no `body`). | Should not happen with stable Chrome; file a bug. |

## `CloudflareError` variants

Gated `cloudflare`. Sub-error returned wrapped in
`ZendriverError::Cloudflare`.

| Variant | Common cause | Fix |
|---------|-------------|-----|
| `NoChallenge` | No Turnstile iframe was detected at call time. | Treat as success — the page was already cleared (cookie shortcut) or had no CF gate. |
| `ClearanceTimeout` | Deadline elapsed without resolution. | The challenge may be silent / escalated; check the stealth profile (see [Stealth](./stealth.md#when-to-use-which)), or switch to a residential proxy. |
| `Call(CallError)` | Underlying CDP call failed (typically the JS detection probe). | Inspect inner error. |
| `JsError(String)` | The detection / clearance JS raised an exception. | Inspect the exception. (The page's CSP isn't the cause: zendriver runs this JS through DevTools, which CSP doesn't restrict.) |

## `FetcherError` variants

Gated `fetcher`. Sub-error returned wrapped in `ZendriverError::Fetcher`.

| Variant | Common cause | Fix |
|---------|-------------|-----|
| `Http(reqwest::Error)` | Network call to the CFT manifest / CDN failed. | Check connectivity; CFT URLs need outbound HTTPS to `googlechromelabs.github.io` and the CDN. |
| `Io(io::Error)` | Local FS write failed (cache, extract). | Check cache-dir permissions / free space. |
| `Manifest(serde_json::Error)` | Manifest JSON didn't parse. | Should not happen with the canonical URL; means the CFT side changed format — file an issue. |
| `VersionNotFound(version)` | `VersionSpec::Explicit("...")` string not present in manifest. | Drop a version (CFT only keeps the last N); use `VersionSpec::Latest` or a known version from the manifest. |
| `UnsupportedPlatform` | `Platform::auto_detect` returned `None`, or the resolved manifest/channel has no download for the requested platform. | Currently no fix for unsupported platforms (Linux arm64, BSDs); install Chrome out-of-band. |
| `IntegrityFailed { expected, actual }` | SHA256 of the downloaded zip doesn't match the manifest. | Delete the partial download under the cache dir; retry. |
| `Extraction(String)` | Zip extraction failed. | Free disk space; check for filesystem corruption. |

## Pattern-matching tips

- **Use `matches!`** for boolean checks on a single variant:
  ```rust,ignore
  if matches!(err, ZendriverError::ElementNotFound { .. }) {
      // soft-fail path
  }
  ```
- **Use `_` always** to handle future variants gracefully — `#[non_exhaustive]`
  requires it.
- **Sub-errors flatten via `#[from]`** — your `?` operator works across
  the boundary (e.g. `let body = paused.body().await?;` returns
  `InterceptionError` but converts to `ZendriverError::Interception`
  inside a function returning `zendriver::Result`).
