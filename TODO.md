# TODO

Bugs from the 2026-10-31 AI review ([ai-code-reviews/](ai-code-reviews/README.md)) and from running the library against real sites ([R](<ai-code-reviews/Project testing on ludios.org and sites.md>)), turned into work items.

**Order:** P0 → P1 → P2. The sections outside P0–P2 have no set place in this order.

**Rule:** every fix lands with a test that fails before the fix. Some of those tests exist already: a test named `known_bug_*` is `#[ignore]`d with its TODO item in the reason, and the fix deletes the `#[ignore]` and the prefix. A stealth fix deletes its row from `KNOWN_DISAGREEMENTS` in `zendriver-testkit`'s `probe.rs` or `KNOWN_BUGS` in `stealth_conformance.rs` (the test fails until it does). If a behaviour depends on Chrome, test it against real Chrome, not the mock CDP. About a fifth of the review findings are only "likely". When a test can't reproduce one, close it as not-a-bug rather than fixing blind.

**Source tags** (`#n` = finding number in that report):
- **R:** confirmed by running against real sites.
- **BR:** [browser](ai-code-reviews/zendriver-browser.md)
- **TAB:** [tab](ai-code-reviews/zendriver-tab.md)
- **QE:** [query/element](ai-code-reviews/zendriver-query-element.md)
- **FI:** [frame/input](ai-code-reviews/zendriver-frame-input-capture.md)
- **MEC:** [monitor/expect/cookies](ai-code-reviews/zendriver-monitor-expect-cookies.md)
- **ST:** [stealth/fingerprints](ai-code-reviews/zendriver-stealth-fingerprints.md)
- **MCP:** [mcp](ai-code-reviews/zendriver-mcp.md)
- **TI:** [transport/interception](ai-code-reviews/zendriver-transport-interception.md)
- **FW:** [fetcher/WAF/gen](ai-code-reviews/zendriver-fetcher-waf-gen.md)

**Test tags:**
- `chrome`: real-Chrome test in `crates/zendriver/tests/` against the local fixture server (`zendriver-testkit`).
- `mcp`: real-Chrome test in `crates/zendriver-mcp/tests/` that drives the server.
- `probe`: covered by the stealth consistency probe (`stealth_probe.rs`).
- `unit`: no browser.

---

## Test harness

**Local setup**, on the NixOS machine that AGENTS.md describes: `~/bin/cargo` there forces nightly-only `-Z` flags, but the repo pins stable, so run `/run/current-system/sw/bin/cargo`. `cargo-insta` isn't installed: accept a schema snapshot by moving its `.snap.new` over the `.snap` minus the `assertion_line:` line, or run the test with `INSTA_UPDATE=always`. `mdbook` isn't on PATH: `nix shell nixpkgs#mdbook -c mdbook build docs/book`. `cargo-public-api` 0.52.0 and `nightly-2026-06-10` are installed (built with `nix-shell -p openssl pkg-config --run "cargo +stable install cargo-public-api --locked --version 0.52.0"`), so regenerate the baseline with `cargo +nightly-2026-06-10 public-api -p zendriver --all-features > crates/zendriver-mcp/public-api-baseline.txt` and check with `PUBLIC_API_TOOLCHAIN=nightly-2026-06-10 cargo +nightly-2026-06-10 test -p zendriver-mcp --features public-api-check --test public_api --locked` (the default nightly reports hundreds of unrelated items).

- [ ] **No CI job runs the `known_bug_*` tests.** The nightly ignored-test lane skips them, since they fail by design. So one that starts failing for another reason, or starts passing because something else fixed it, goes unnoticed. An inverted nightly check (alert when one passes) would close that.
- [ ] **CDP contract test (`cdp_contract.rs`) limits:**
  - Scopes are hand-kept, and the source scan compares only names. A new browser-scope call to a method that already has a page row isn't caught. The stronger version records the `(method, has session)` pairs actually sent during the integration suite, via a transport hook, and checks those against Chrome.
  - Quoted names whose domain isn't a CDP domain are ignored, so a misspelled domain slips through. Flagging them would need an allowlist of about 13 non-CDP names (`Math.abs`, `Chromium.app`, mock placeholders).
- [ ] **`ChromeTree` is unverified on Windows**, which is the leg that runs `repeated_launch_close_leaves_no_orphan_chrome_processes`. `ChromeTree` reads the browser's `--user-data-dir` from its command line via `sysinfo`, and it relies on `SystemInfo.getProcessInfo`. Watch the first Windows CI run.

---

## Decided, not yet implemented: honeypots and `visible_only`

Decided on 2026-11-01 by an Astra–Fable consensus round (three rounds; both signed). The full record, with the signed text, is `ai-code-reviews/honeypot-visible-only-consult/` (`decision.md`; `brief.md` has the real-Chrome probe over 20 hiding patterns and the corpus data; `survey-summary.txt` is the live-site survey: of 25 off-page rendered inputs on 239 pages, 21 were Mailchimp honeypots, and the withdrawn geometry rule would have caught 12 and exempted 13 through an `overflow: hidden` body or wrapper, while the 4 non-Mailchimp ones were legitimate off-canvas search boxes). Implement `decision.md` as written; in short:
- The filter and `is_visible` stay "rendered": no geometry, no level enum, no `is_on_page`. Rust default `visible_only(false)`, MCP default `true`.
- New `Element::is_in_viewport()` (= `check_visible(Within::Viewport)`, no scroll), filling `browser_element_state`'s `in_viewport` for the `all` / `geometry` / `visible_enabled` presets.
- New `Element::wait_until_in_viewport_and_enabled(timeout)`: `scroll_into_view` + the TEXT_INPUT poll, no focus; `focus()` = that + focus. Library `set_value` / `clear` / `set_text` stay ungated, with rustdocs pointing at the new method as the opt-in gate. MCP `browser_set_value` (both modes), `browser_clear` (value mode) and `browser_type` with `clear_first` run it first unless `unchecked: true`; backspace-mode clear rejects `unchecked`.
- `wait_actionable` re-scrolls before the next poll only after a failed visible check; no fast-fail.
- `NotActionable` becomes a struct `{ timeout, reason, bbox, viewport }` with `NotActionableReason::{NotRendered, OutsideViewport, Disabled, Unstable, HitTestMissed { at, hit }}` and the display strings in `decision.md`. Breaking: regenerate the public-API baseline.
- MCP on `NotActionable`: one re-query (same selector / frame / `visible_only`, no `nth`, timeout 0, limit 10); the count, up to two other matches by node identity as bounded descriptors, and the suggestion to narrow the selector or list matches with `browser_find_all` before choosing another `nth`. The `browser_html` hint goes.
- Action tools: optional `in_aria_hidden_subtree: true` (omitted when false); always present in the failure descriptors.
- Documented limits: clipped and far-below honeypots remain actionable.
- With the implementation commit: a new entry in `docs/api-audit/decisions.md` superseding #1's fork note, and the CLAUDE.md checklist (ledger, baseline, snapshots, README, book). Real-Chrome tests from the probe's fixture page: a honeypot before the real input, per hiding pattern (`honeypot_probe.rs` in the record folder).

Also for this round:
- From review: `display: contents` at `opacity: 0` may still be rejected by `checkVisibility` (Mythos, "most likely").
- The opacity walk can't see a closed shadow root's slots (`assignedSlot` is null from page script), so a faint wrapper there is skipped (documented).

---

## P0: security, hangs, and features that never work

### Interception
- [ ] **HTTP auth, left open:**
  - Proxy auth: an isolated context without a proxy of its own uses the browser's proxy but gets none of `proxy_auth`'s credentials (giving them would need the context builder to record whether the context has its own proxy).
  - A site's own HTTP auth (a 401 from the site, not a 407 from a proxy) has no helper, and no doc says how to answer it, e.g. by adding an `Authorization` header with `modify_request`.
- [ ] **Interception: review findings left open** *(reviews of `222fe294b..907f3a7f2`, 2026-11-06)*
  - An overflow of the multiplexer's queue of `Fetch` events (4,096 of this session's `Fetch` events unread when another arrives, which takes the multiplexer blocked meanwhile: waiting on Chrome in `apply` or `recover`, or running a slow `modify_request` / `modify_response` closure): the recovery's `Fetch.disable` continues every paused request, those a block rule or a stream would have held included. Chrome bounds the paused requests, so this queue could do without a bound of its own (it would need a per-subscription capacity in the transport); running closures and round trips beside the event loop would help too.
  - A stream layer's queue is unbounded (Chrome holds the requests anyway); the auth memory doesn't know which credentials answered, so new credentials for the same request and proxy within 60 s get `CancelAuth`.
  - *Open from the reviews of `b965a0d09..777c35159`, `4dd5e05e0` and `b357bdb23` (2026-11-06):*
    - An entry inserted while a request is already under way (paused at the Request stage, or sent before `insert` returned) doesn't get that request's Response pause: Chrome fixed its stages when it started. Documented as "a request sent from then on". Sending `interceptResponse` on every Request-stage continue, worked out (by resource type, as for a new URL since `94768fd1e`) from the list at answer time, would close it for a request still paused at the Request stage, but not for one already sent.
    - When Chrome refuses both an answer and the `failRequest` that follows it, the hold is dropped as answered, so a later `Fetch.disable` may release the request (if Chrome still had it). Keeping the hold would keep `Fetch` on for a request Chrome most likely no longer knows. *(Astra, `b965a0d09..777c35159`.)*
    - A `Fetch` pattern also pauses prefetches, which the multiplexer reads as `Other`, so a `Fetch` entry doesn't fit them and they're continued unchanged.
    - Registrar: two attaches of one target handled at once can still build two tabs; under the flat-session fallback, moving the registry key to the new session leaves the kept `Tab` on its old session, and `on_target_detached` removes by key.
    - A new tab with both proxy credentials and tracker blocking makes two `Fetch.enable` round trips before it runs.
    - MCP: `origin` is worked out by elimination (an entry not in `SessionState::rules` is `browser`); an owner tag on the core `Entry` would be firmer. `list_rules` shows the current tab only, while `remove_rule` / `clear_rules` reach every tab. `add_rule` returns an error if the listing after the insert fails (the rule stays in, its id unreported). `clear_rules` stops the handles one by one (a round trip each). `ResourceTypeName` / `PauseName` mirror core enums by hand. `hosts` isn't IDN-normalized; `respond` takes any status code; `modify_request` takes forbidden header names.
    - `docs/api-audit/interception-precedence.md` is the design record as agreed, not the API as built: its signatures leave out `Entry` / `UrlInfo` / `ActionKind`, and its `remove(tab.tracker_blocking())` needs an unwrap.
  - MCP: `browser_tab_close` drops the rules of the tab it closes, but a tab that closes in another way (e.g. the page's `window.close()`) leaves its rules in `SessionState::rules` until `browser_intercept_clear_rules` or `browser_close`. They do nothing (their tab's list is gone), but `browser_intercept_remove_rule` still accepts their ids.
- [ ] **Interception misses out-of-process iframes.** *(found 2026-11-01, reading Chromium)* A frame's requests go through the `Fetch` handlers of that frame's own agent host plus every browser target's (`content/browser/devtools/devtools_instrumentation.cc:2041`), so `Fetch` on a page session doesn't see an OOPIF's requests: tracker blocking, proxy auth and user rules all miss third-party iframes. Fix candidate: give each attached `iframe` session (the registrar's `iframe` arm) its own multiplexer with the library's layer. Test `chrome`: a cross-site iframe (`/iframe-cross-site`, with site isolation) whose tracker request is blocked.

### Navigation waiting (biggest real-world pain: R1)
- [ ] **Load tracking, left open** *(R1, TAB #1, TAB #2)*
  - `back()` to a history entry where only an iframe navigated: the main frame never loads, so `wait_for_load` times out after 30 s (MCP `browser_back` too, unless `wait_for: none`).
  - `new_tab_at` takes as long as the new page keeps its main thread busy (a 2 s busy loop, even one scheduled with `setTimeout(0)`, made it take 2 s), with or without the load tracker: something in the attach chain waits on the renderer.
  - `wait_for_ready_state` right after `goto` can read the old document: `Page.navigate` replies before the commit.
  - `goto(A)`; `goto(B)`: B's tracker can still end on A's stop when A's start and stop come after B's tracker subscribed. Checking the destination at every stop would fix it, but would break downloads (no commit) and "a navigation that began after it".
  - The `readyState` probe of a tracker for the current document (`wait_for_load` with no navigation pending) or for a new tab blocks while a JavaScript dialog is open, so the wait can run to its timeout then.
  - **A crashed tab.** After `Page.crash`, the tab's session answers no renderer command, even after `Page.reload` (which returns, and in about half the runs brings the target back to its URL within 4 s; in the others its URL stays empty for over 10 s). `wait_for_load` after reloading a crashed tab times out or returns early. Chrome sends `Page.frameStartedLoading` / `frameStoppedLoading` from the browser process (`PageHandler::DidChangeFrameLoadingState`); `Inspector.targetCrashed` didn't arrive on the tab's session. The test `reload_of_a_crashed_tab_needs_no_renderer` in `navigation_waits.rs` checks only that `reload()` returns; no test calls `wait_for_load` after it.
  - Cost: an armed tracker is a task, with a subscription to its session's two loading events, until the next wait or navigation. Also, `probe_complete` sends `Page.enable` every time.

### Broken outright
- [ ] **MCP shutdown leaves Chrome running.** *(R, MCP #6)* After a SIGTERM, the server closes Chrome over both transports (`chrome_exits_on_sigterm_over_*`).
  - SIGKILL (`known_bug_chrome_exits_on_sigkill_over_*`) needs a guard on Chrome's side. Linux `PR_SET_PDEATHSIG` in a `pre_exec` is the simplest, but it fires when the *thread* that spawned Chrome exits, not the process: a launch from a `spawn_blocking` thread (which exits after 10 s idle) or a short-lived thread would kill Chrome early. A watchdog (a pipe Chrome's helper reads, or a tiny reaper process) avoids that. Chrome has that pipe built in: launched with `--remote-debugging-pipe`, which it accepts alongside `--remote-debugging-port` (`RemoteDebuggingServer::GetInstance`), it calls `CloseBrowserSoon` when the pipe on fds 3 and 4 reaches EOF (`StartPipeHandler` in `chrome/browser/devtools/remote_debugging_server.cc`), as it does when our process dies, however it dies. Holding the pipe open without speaking CDP on it keeps the WebSocket transport as it is (checked on Chrome 157: the port works, and Chrome exits with status 0 once the pipe closes). Either way it's a library change for every user, so decide whether it's opt-in.
  - Over HTTP, after a SIGTERM, a tool call that holds a session's lock delays that session's cleanup, and if it holds the lock for over 20 s, the server gives up on the cleanup and exits without having closed that session's Chrome.
  - From review: after the shutdown signal, other MCP tools still answer "browser not open" with a hint to call `browser_open`, which then refuses (the server exits within seconds).

---

## P1: wrong results

### Frames and elements
- [ ] **Stale-handle refresh of a frame element re-runs the selector on the top-level page.** Re-resolve inside the frame instead. *(QE #2)* Test `chrome`.
- [ ] **`best_match` handles refresh to a different element.** *(QE #9)* Test `chrome`.
- [ ] **OOPIF fixes.** OOPIFs attach since 2026-11-14 (each page's session auto-attaches them, see `ChildTargetAutoAttach`), and get the stealth setup. But `tab.frames()` / `frame_by_url` still don't show them: after the load of a page with a cross-site iframe, `tab.frames()` lists only the main frame (Chrome 157). Every profile makes OOPIFs (site isolation is on, see "Launch flags" under Stealth), so all of these are reachable.
  - keep the frame's entry when the iframe becomes remote. `register_oopif_frame` does run for the OOPIF (it logs no warning), so presumably the parent's `frameDetached` with reason `swap` then removes the entry *(FI #5)*. Nothing follows the OOPIF session's own `Page.frame*` events either, so such an entry's URL would stay empty.
  - elements bound to the OOPIF's session *(QE #3)*
  - parent id *(FI #4)*
  - a `tab.set_user_agent` after launch isn't sent to the sessions of the tab's OOPIFs, which keep the launch's user agent
  - `connect()` returns before the observer chain has run on the OOPIFs of the tabs that are open already
  - in-flight request leak in the idle tracker *(TAB #5)*

  Test `chrome` with the cross-site fixture: `launch_for_test(Browser::builder())`, then goto `server.url(SITE_A, "/iframe-cross-site")`.
- [ ] **`set_value` / `clear` throw on `<select>` and non-input elements.** Pick the setter that matches the element's own prototype. *(QE #5)* Test `chrome`.
- [ ] **`check_stable` waits on `requestAnimationFrame` with no timeout.** Bound the wait. *(QE #10)* Test `chrome` with a background tab.
- [ ] **An isolated-world context id can still name another context, for one command.** Zendriver asks for the id with `Page.createIsolatedWorld`, then sends the command that uses it. A navigation into another renderer process in between can give the id to a context of the new page (an iframe's, say), and the command then runs there with no error. `uniqueContextId` would make Chrome refuse it, but Chrome reports that only in `Runtime.executionContextCreated`, which needs `Runtime.enable`. *(reviews of 6c6593aa4)*
- [ ] **A navigation during a query's poll ends `.one()` instead of letting it poll on.** `eval_expr_in_scope` retries only `Cannot find context` from its `Runtime.evaluate`. `Inspected target navigated or closed`, and stale errors from the `Runtime.getProperties` / `DOM.describeNode` that read the result, still end the query. Treat them as "no match yet" in the poll loops. Test `chrome`: `.one()` polling while the page navigates repeatedly. *(reviews of b0e5a49cc)*

### Selectors
- [ ] **Text selectors pick the wrong element.**
  - `text_exact()` returns the outermost match (a table cell, not the link inside), so the click lands on nothing yet reports success. *(R2)*
  - Text selectors also match `<title>`, `<script>` and `<style>`. *(QE #7)*
  - Prefer the deepest rendered match. Test `chrome`: table-with-link fixture.

### Downloads, expect, monitor, cookies
- [ ] **Downloads, phase 2** (phase 1, 2026-11-06: downloads followed at browser scope, so a `MatchedDownload` outlives its tab; designed with Astra and an Opus critique):
  - `Browser::expect_download` (or popup attribution): a download in a tab that Chrome opened for it (a `target=_blank` link to a file) matches no `expect_download` today. Playwright attributes a download of a page that hasn't loaded its first document to its opener.
  - MCP download ids: a matched download kept in the session, with status / save / cancel / release tools. Today the result names the file (`path`, once the download has completed), but the handle is dropped, so no tool can wait for, save, or cancel a download that is still under way.
  - `MatchedDownload::cancel` (`Browser.cancelDownload { guid, browserContextId }`).
  - Deleting a download's file when its last handle drops: only after the download ends (Chrome writes `<guid>.crdownload` and renames it at the end), so a crawler with a tab per download doesn't fill the disk until the `Browser` drops.
  - `connect()`: an "observe" mode (`behavior: "default"` with `eventsEnabled`, the file found by `filePath`) might touch a person's Chrome less than sending its downloads into the staging directory.
  - If Chrome drops the deprecated `Page.downloadWillBegin`, attribute by `Browser.downloadWillBegin`'s `frameId` instead (main frame = the tab's target id; subframes need a reliable frame index, which today's OOPIF registry isn't).
  - `BrowserContext::set_download_path`, and `set_download_path` exporting completed files by copy (Astra's design) if `filePath` (Chrome 138+) turns out not to be enough.
- [ ] **`expect_response(...).body()` reads before the body has arrived.** Wait for `loadingFinished`. *(R4, MEC #7)* Test `chrome` with the trickled-body fixture.
- [ ] **File-chooser intercept left on after an error.** Disable it on the error path. *(MEC #4)* Test `chrome`.
- [ ] **Monitor: `stream_bodies` chunks can arrive out of order.** *(MEC #6)* Test `chrome`.
- [ ] **From the 2026-11-06 reviews of the detach work (older problems, not fixed):**
  - A monitor's body-streaming tasks hold its output channel, so chunks can follow its terminal `Shutdown` / `Disconnected` / `Reconnected` boundary, and the stream doesn't return `None` until they finish.
  - Interception: dropping the last `Interceptor` and rule handles closes the command queue while an answer task (which holds only a weak sender) is pending, so `Mux::close` disables `Fetch` before a rejected `Fetch.fulfillRequest`'s fallback `Fetch.failRequest` runs (reproduced with `MockConnection` by a reviewer).
- [ ] **`get_all_urls(true)` fails on SVG `href`.** *(TAB #4)* Test `chrome` with the SVG sprite fixture.
- [ ] **Cookies, left open:**
  - `for_url` without the public suffix list: a cookie partitioned under a public suffix with a dot (a page at `github.io` itself) is returned for that suffix's subdomains. `psl` is in the tree (via `zendriver-datadome`) but not in the default build.
  - From review: `only_isolated_tab_jars_name_their_context` reads the jar's `Debug`; `tab_cookies_write_into_the_tabs_context` can't tell the tab's connection from the browser's.

### Browser lifecycle
- [ ] **`connect()` builds the fingerprint from an empty executable path.** *(BR #4)* Test `chrome`. (BR #3, that it patched pre-existing tabs contrary to its docs: its docs now say that it sets them up as tabs, which it does since 5409b2a92.)
- [ ] **`new_tab_at` with `chrome://` / `chrome-extension://` URLs stalls and returns `TabSetup`** (with the reason that the setup ran out of its budget), as the registrar doesn't set up such a page as a tab. *(BR #5)* Test `chrome`.
- [ ] **`geo_auto()` captures the proxy at call time.** Calling `.geo_auto().proxy(..)` in that order probes from the real IP. *(BR #6)* Test `unit` on builder order.

### Transport and input
- [ ] **One socket for every target: stages 2 and 3 of the event model** *(decided 2026-11-08)*. Stage 1, ordered per-session event queues (`SessionHandle::events`), is done. Stage 2: OOPIFs attach on their page's session since 2026-11-14; a `Frame` for each of them is still to do (see "OOPIF fixes" above). Stage 3: run every target as a flat session on the browser's socket, and delete `per_tab_session` and its fallback. The per-tab socket makes stealth's setup during the debugger pause slower (no pipelining across two sockets), turns prerender off, and lets `tab.set_user_agent` beat stealth's override whatever the order of the writes (Chrome applies emulation in the order that sessions attached).
- [ ] **An observer's failure on a child target can't detach it.** *(found 2026-11-14, by reviewers of `d52d2a584`)* `detach_target` sends `Target.detachFromTarget` on the browser's session, which doesn't know a session that was attached through a page's session (an OOPIF's, a worker's): Chrome 157 answers `-32602`, "No session with given id", and the actor drops that error. So the target stays attached, and paused. Send the detach on the parent session (the `sessionId` of the `Target.attachedToTarget` event). First find out whether Chrome then lets the target run, which it mustn't without its setup. Test `unit`.
- [ ] **`Routes::detached` grows by one id for every session that ends**, until a reconnect: once per closed tab before, and since 2026-11-14 also once per OOPIF, dedicated worker and worklet. Forget the ids that nothing can subscribe to any more (e.g. a child's, once its parent's session has ended too). Test `unit`.
- [ ] **A reply frame that fails to parse hangs its caller for 180 s.** Fail the pending caller instead. *(TI #9)* Test `unit`.
- [ ] **An event frame that fails to parse is dropped without a trace on its session's streams.** `serde_json`'s recursion limit (128) rejects valid deep events (a reviewer saw `DOM.setChildNodes` for a DOM 70 elements deep fail on Chrome 157); the actor logs a warning and drops the frame, and `SessionHandle::events` goes on with the next event as if none were missing. Parse without the limit (`serde_json`'s `unbounded_depth`, with `serde_stacker` against stack overflow). Test `unit`: replay a deep frame.
- [ ] **A socket write that never completes keeps the actor from noticing its shutdown**, so neither its pending calls nor its session streams end. *(found reviewing `6fbfdd6cd`)*
- [ ] **Grapheme clusters longer than 4 UTF-16 units fail.** Split into insertText or per-codepoint events (ZWJ emoji, flags, skin tones). *(FI #3)*
  - Test `chrome`: `type_text("👩‍💻🇯🇵👍🏽")` round-trips.

### Stealth
These are mostly covered by `probe` (`stealth_probe.rs`) / the conformance test (`stealth_conformance.rs`), plus `unit` where noted. A fix deletes the bug's row from `KNOWN_DISAGREEMENTS` in `zendriver-testkit`'s `probe.rs` (shared by `stealth_probe.rs` and MCP's `real_chrome.rs`) or `KNOWN_BUGS` in `stealth_conformance.rs`.
- [ ] **Launch flags: site isolation is never disabled.** Found 2026-10-31 (Chrome 156).
  - `--disable-features=site-per-process` does nothing; the feature is called `SitePerProcess`. `--disable-site-isolation-trials` also works.
  - Decide whether site isolation should really be off. The profile docs say it is, but turning it off now changes behaviour for every user: cross-site iframes would stop being OOPIFs.
  - Test `chrome`: count the `iframe` targets on `/iframe-cross-site`, as `cross_site_iframe_is_out_of_process_under_site_isolation` in the testkit's `fixtures.rs` does.
- [ ] **An OOPIF can lose its locale, timezone and focus emulation when another iframe goes.** *(found 2026-11-14, by reviewers of `c061f8f44`)* A renderer process holds one locale override and one timezone override for all of its targets, and they belong to the session that set them first; focus emulation likewise belongs to one session per process and tab. When that session ends (its iframe is removed), the process's other iframes (those of the same site) are back to the host's locale and timezone, and without focus emulation: a reviewer saw the surviving iframe go from `de-DE` / `Europe/Berlin` to `en-US` / `UTC` (Chrome 157). Fix candidate: when an iframe's session ends, send those overrides again on the remaining iframe sessions of its tab. Test `chrome`: two iframes of one site (as in `child_targets.rs`), remove the first.
- [ ] **Chromium and Chrome for Testing run Chromium's field-trial testing config**, which Google Chrome doesn't (`testing/variations/README.md` in Chromium's source): hundreds of features in the state that Chromium is testing. One of them is page-visible for sure: `SingleVideoFrameRateThrottling` halves `requestAnimationFrame` to 30 Hz while one 30 fps video plays. Measured 2026-11-14 with Google Chrome 157, where the feature is off (rAF stays at 60 Hz) and `--enable-features=SingleVideoFrameRateThrottling` turns it on: 30 Hz, headless and headed (under Xvfb) alike. Upstream's `793d8afc6` disables that one feature, in headless only. Playwright passes `--disable-field-trial-config`, which turns the whole config off (Puppeteer added that switch, then removed it). Not measured: either fix on a Chromium or Chrome for Testing build, which this needs before a flag goes in. Test `chrome`, with a Chrome for Testing build: rAF's rate beside a 30 fps `<video>`.
- [ ] **JS-visible and HTTP identity come from different sources.** One identity source should feed JS, the UA, UA-CH and headers. *(ST #1, R6)*
  - Under `spoof_windows` on Linux, the UA, the UA client hints, the headers and the WebGL GPU say Windows, but `navigator.platform` says Linux in the page and in workers.
  - **The page's `navigator.platform`:** Chrome keeps the platform of `Emulation.setUserAgentOverride` in the page's settings. Every DevTools session's emulation agent rewrites that setting with its own platform (empty if it set none) when it's restored after a cross-process navigation and when it detaches. So the tab's own socket, a second session that sets no override, wipes the stealth session's: under `native().platform(Win32)`, `navigator.platform` is `Win32` on `about:blank` and `Linux x86_64` after the first navigation and in new tabs (Chrome 157, 2026-11-07). With one raw CDP session, it stays `Win32`. Fix it by sending the same override on every session zendriver opens on a page, or with flat sessions.
  - **A dedicated worker's `navigator.platform`** is the host's (`NavigatorBase::platform`): no override reaches it.
- [ ] **Worker GPU name, `deviceMemory` and `hardwareConcurrency` differ from the page under `spoofed`.** No stealth patch or override reaches a worker, and `KNOWN_DISAGREEMENTS` files all three under this item. *(R6)*
  - A reviewer of `9f0372ace` (2026-11-07) attached a dedicated worker in Chrome 157 and sent it `Emulation.setHardwareConcurrencyOverride`, and the worker's own getter then reported the persona's count. So page-session auto-attach of dedicated workers, with the observer applying the CPU-count override to them before they run, would close the `hardwareConcurrency` part.
- [ ] **Canvas farbling turns transparent pixels into `(1,1,1,0)`.** *(ST #2)*
- [ ] **macOS UA-CH `platformVersion` / `architecture` describe an impossible machine.** *(ST #3)*
- [ ] **`chrome_version(major)` invents a build number.** *(ST #4)* Test `unit`.
  - Also, with that pin, `navigator.userAgentData.getHighEntropyValues(['uaFullVersion'])` reports the binary's full version, because the `userAgentMetadata` that the observer sends has no `fullVersion`. Seen by a reviewer of `904406eb9` with Chrome 157.
- [ ] **Linux UA-CH `platformVersion` is `5.15.0`** in every profile's `userAgentMetadata` (`UserAgentMetadata::realistic`). Chrome reports an empty string on Linux (`GetPlatformVersion` in Chromium's `components/embedder_support/user_agent_utils.cc`; a reviewer saw it empty in Chrome 157), so the value is one no Linux Chrome sends. Test `unit`, and the probe.
- [ ] **A screen narrower than 500 CSS px gets a 500-px window** under `native` and `spoofed` (headless Chrome's minimum window width): with `ScreenSpec::new(390, 844, 1.0)`, a reviewer of `9fce51453` saw `screen.width` 390 beside `outerWidth` and `innerWidth` 500 (Chrome 157). Reject such a screen in `headless_screen_switches`, or document the limit. Test `chrome`.
- [ ] **`measureText`'s noise shows after one call.** *(found 2026-11-14, by a reviewer of `8df2b622e`)* Chrome's text widths are whole multiples of 1/65536 px (checked with four fonts on Chrome 157), and a noised width almost never is. `client_rects.js` has the same flaw (Chrome's layout unit is 1/64 px). And an `OffscreenCanvas` 2D context isn't patched, so the same text has another width there. Test `chrome`, in `stealth_conformance.rs`.
- [ ] **`getClientRects()` returns an `Array`,** not a `DOMRectList`. *(ST #11)*
- [ ] **Masked functions are constructible** in the spoofed patches: each is an ordinary `function`, with an own `prototype` (rebrowser's `nativeTampering` checks for one). *(ST #12)*
- [ ] **Own properties on instances** where Chrome has them on the prototype: `hardware.js` patches `navigator`, `navigator.mediaDevices` and `speechSynthesis`, when a persona sets their values. *(ST #13)*
- [ ] **Generative personas use Safari/Firefox UAs.** Filter to Chrome UAs only. *(ST #15)* Test `unit`.
- [ ] **Generative network cache:** validate before writing, and key the cache by URL. *(ST #16)* Test `unit`.
- [ ] **Geo timezone table (`locale-gen`'s "first row wins").** Germany gets `Europe/Zurich`, Netherlands gets Brussels, Iceland gets `Africa/Abidjan`, Vietnam gets Bangkok. Pick each country's principal zone (e.g. from `zone.tab`), then regenerate. *(FW #5, ST #17)*
  - Test `unit`: DE→`Europe/Berlin`, NL→`Europe/Amsterdam`, IS→`Atlantic/Reykjavik`, VN→`Asia/Ho_Chi_Minh`, MY→`Asia/Kuala_Lumpur`.

### Fetcher and WAF
- [ ] **`Stable` / `Latest` resolve to a Canary build.** Resolve through the per-channel CfT endpoint. *(FW #2)* Test `unit` with a fixture manifest.
- [ ] **Cache isn't keyed by platform.** *(FW #3)* Test `unit` with a tempdir and two platforms.
- [ ] **Imperva treats its always-present `incap_ses_*` / `visid_incap_*` cookies as a challenge.** *(FW #4)*
  - Test `chrome`: fixture that sets the cookies on a clean page → reports clear.
- [ ] **The WAF bypasses' `with_interception()` fast path rarely speeds anything up.** *(reviews of `582088ee1`, 2026-11-04)* It fires once, on the first sensor 2xx's headers (`Network.responseReceived`), before the page's script has read the body and set the cookie; the probe then finds nothing, and no later sensor response wakes the loop. The Fetch version behaved the same. A fix: signal on each matched request's `Network.loadingFinished`, re-armable (`watch` / `Notify`), and skip CORS preflights (`type: Preflight`). Also: the two `sensor_signal.rs` copies differ only in their URL parts, and `zendriver`'s `imperva` / `datadome` features still imply `interception`.

### MCP
- [ ] **`browser_tab_close` leaves some of the tab's work running.** *(reviews of `c4b58b531`, 2026-11-06; several reproduced against real Chrome by gpt-6-astra)* Monitors (`MonitorState` has no `tab_id`; `monitor::start` has the tab, so storing it is cheap); an expectation that `browser_expect_await` already removed from the registry; an expectation registered concurrently (the lock is released between resolving the tab and inserting); a tab that closed by itself before the call (`TabNotFound` returns before `drain_tab_scoped`); and the `expect_dialog` subscriber, which outlives its aborted MCP task. (Download expectations are kept on purpose: a matched download outlives its tab.) No test drives `browser_tab_close` with real registrations.
- [ ] **One lock serialises most tool calls.** It's held across CDP calls: a status check waited 3.8 s behind a pending element search, and a dialog can freeze the session. *(MCP #7, R)* (`browser_wait_for_url`, `browser_expect_await`, and the navigation waits of `browser_click` / `browser_press` already release it while they wait.)
  - Test `mcp`: a status call during a pending find returns quickly.
- [ ] **`browser_screenshot` with a selector crops the wrong area on a scrolled page,** and `full_page` overrides `selector`. *(MCP #5, #9)* Test `mcp`.
- [ ] **`browser_type` reports success on elements you can't type into.** *(R)* Test `mcp`.
- [ ] **Tool list is about 215 KB (roughly 50k tokens)** because the selector description repeats across about 15 tools. Share one definition and shorten it. *(R)*
  - Test: a size budget assertion next to the schema snapshots.
- [ ] **Page HTML has no size limit** (305 KB on GitHub, 711–734 KB on YouTube). Add a default cap with a truncation marker plus a `max_bytes` input. *(R)* Test `mcp`. This changes I/O, so regenerate the schema snapshots.
- [ ] **"Suggested next step" hints** aren't where the README says, and many errors have none. *(R)* Test `unit` on the error mapping, plus a docs fix.
- [ ] **Smaller MCP bugs:** *(MCP #11, #12, #14)*
  - a per-launch `stealth_profile` becomes the session default
  - `html_trim` matches tag prefixes and deletes custom elements
  - stale tool descriptions

---

## P2: low severity

Each item needs a test; `unit` is fine where no browser is involved.

- [ ] **Browser:** *(BR #8–#10, #12)*
  - the geo overlay overrides an explicit persona timezone
  - `BrowserContext` `Drop` panics outside a tokio runtime (`unit`)
  - `connect()` ignores tracker blocking. It honors `proxy_auth` since `bc8a71f74`, but no test covers `proxy_auth` through `connect()`: every test in `proxy_auth.rs` launches Chrome.
  - a target that detaches during the registrar stays registered forever
  - closing or dropping a `connect`ed `Browser` shuts down only the browser's own socket, so a `Tab` kept after that keeps its own socket, and its waits and monitors go on. *(Astra, from reading the code; not reproduced.)*
- [ ] **Tab:** *(TAB #6–#9, #11, #12)*
  - the cached `main_frame()` URL never updates (also FI out-of-scope note)
  - `wait_for_idle_opts` can lose a wake-up
  - `search_frame_resources` skips the frame's own document
  - `bypass_cors()` drops user headers
  - the geo probe can't use SOCKS proxies
  - the hosts-file blocklist parser blocks `localhost`
- [ ] **Frame/input:** *(FI #6–#13)*
  - `Key::Char` whitespace
  - Shift held on a lowercase character
  - CRLF never presses Enter
  - stale iframe handle rebinds to a sibling
  - `Frame::url()` stale after pushState
  - frame registry never seeded
  - modifier keys missing `location`
  - `overshoot_rate` unused
- [ ] **Query:** *(QE #12–#14)*
  - element-scoped `.xpath("//…")` searches the whole document
  - `<fieldset disabled>` not treated as disabled
  - polling gives up on transient navigation errors
  - the `lastIndex` resets of `d459e8f1b` have one test, a tab-wide `g` query of elements without children. Nothing tests an element-scoped query (`build_text_regex_fn_body`), the narrowing to descendants (`regex_narrowing_js`: Astra removed its reset, and the test still passed), or the `y` flag.
- [ ] **Monitor/expect/cookies:** *(MEC #9, #11)*
  - subscriber tasks outlive their expectations
  - `Cookie::expires` is `Some(-1.0)` for session cookies
- [ ] **Transport/interception:**
  - detach can run before attach finishes *(TI #13)*
  - the connection's record of detached sessions (`Routes::detached` in `zendriver-transport`) gains one session id per detach and loses them only at a reconnect, so it grows for the life of the connection's socket. Bound it, e.g. to the sessions that some `SessionHandle` still refers to.
- [ ] **Fetcher/WAF:** *(FW #6–#10)*
  - zip symlink directory creation (`unit` with a crafted zip)
  - partial downloads left behind
  - `Done` progress event fires early
  - Imperva `AlreadyClear { reese84 }` unreachable
  - `inject_captcha_solution` escaping
- [ ] **Stealth:** *(ST #18–#23)*
  - WebGPU architecture wrong for 5 NVIDIA workstation cards
  - u64 seed loses precision in JS
  - `nearest_to_renderer` sibling match
  - `UaSpec` fields never applied
  - `deviceMemory` floor

---

## Docs

- [ ] **The README's first example and `crates/zendriver/examples/hello.rs` fail** because example.com was redesigned. *(R)* Point them at a stable target and add an examples smoke job (almost every example hits a live site, so that job needs stable targets first).
