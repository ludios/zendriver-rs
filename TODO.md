# TODO

Bugs from the 2026-10-31 AI review ([ai-code-reviews/](ai-code-reviews/README.md)) and from running the library against real sites ([R](<ai-code-reviews/Project testing on ludios.org and sites.md>)), turned into work items.

**Order:** Phase 0 (test harness) first, then P0 → P1 → P2.

**Rule:** every fix lands with a test that fails before the fix. Many of those tests exist already (Phase 0): a test named `known_bug_*` is `#[ignore]`d with its TODO item in the reason, and the fix deletes the `#[ignore]` and the prefix. A stealth fix deletes its row from `KNOWN_DISAGREEMENTS` in `zendriver-testkit`'s `probe.rs` or `KNOWN_BUGS` in `stealth_conformance.rs` (the test fails until it does). If a behaviour depends on Chrome, test it against real Chrome, not the mock CDP. About a fifth of the review findings are only "likely". When the Phase 0 test can't reproduce one, close it as not-a-bug rather than fixing blind.

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
- `probe`: covered by the stealth consistency probe (0.5).
- `contract`: covered by the CDP contract test (0.4).
- `unit`: no browser.

---

## Phase 0: real-Chrome test harness

Real-Chrome tests run per PR on Linux (`integration-tests` feature), and nightly runs the `#[ignore]`d ones. Most of the logic is still covered only by the mock CDP (418 `expect_cmd` sites), which is how real-Chrome bugs like the browser-level cookie bug got through. The 107 hand-rolled `Browser::builder()` calls and 36 ad-hoc `MockServer`s in the existing tests can move to the shared fixtures (0.1) as they are touched; there's no need to migrate them all at once.

**Status (2026-10-31):** all done. The external-site checks and the examples smoke job from 0.9 were dropped on purpose (see 0.9).

**Local setup:** `~/bin/cargo` on this machine forces nightly-only `-Z` flags, but the repo pins stable, so run `/run/current-system/sw/bin/cargo`. On rustc 1.99, `cargo clippy --workspace -D warnings` fails in `zendriver-transport` on `clippy::double_must_use` from `#[async_trait]`, which predates this work (upstream has a branch `fix/clippy-1.99-double-must-use`). Until that merges, add `-A clippy::double-must-use` to check everything else. `cargo-insta` isn't installed: accept a schema snapshot by moving its `.snap.new` over the `.snap` minus the `assertion_line:` line. `cargo-public-api` 0.52.0 and `nightly-2026-06-10` are installed (built with `nix-shell -p openssl pkg-config --run "cargo +stable install cargo-public-api --locked --version 0.52.0"`), so regenerate the baseline with `cargo +nightly-2026-06-10 public-api -p zendriver --all-features > crates/zendriver-mcp/public-api-baseline.txt` and check with `PUBLIC_API_TOOLCHAIN=nightly-2026-06-10 cargo +nightly-2026-06-10 test -p zendriver-mcp --features public-api-check --test public_api --locked`.

- [x] **0.1 Shared fixtures:** the unpublished crate `crates/zendriver-testkit` (a path-only dev-dep, so `zendriver-mcp` can use it too). Its own real-Chrome tests are in `crates/zendriver-testkit/tests/fixtures.rs`.
  - `FixtureServer::start()` / `start_with(router)`: one axum server for every route, including the trickled body. The routes are listed in the `FixtureServer` rustdoc. Every request is logged with its host, headers, arrival time and response time (`requests()`, `requests_to(path)`, `last_responded_at(path)`).
  - Cross-site hosts: `SITE_A` = `a.test`, `SITE_B` = `b.test`. `/iframe-cross-site` reads `Host` and embeds the other site.
  - `launch_for_test(builder)` → `TestBrowser` (derefs to `Browser`): headless, `--host-resolver-rules`, and Chrome's default temp profile. `browser.logs()` (`at_least(Level::WARN)`, `assert_none_at_least`) is everything the test thread logged since that launch; each thread has one capture. It needs a current-thread runtime, which is `#[tokio::test]`'s default.
  - `close()` closes with the last `Browser` handle, then asserts that this Chrome's whole process tree exited (`ChromeTree`). Dropping a `TestBrowser` unclosed panics, so fail tests with `expect`, not `?`.
  - `ChromeTree` doesn't track crashpad's second handler (its monitor), because only a socket ties it to this Chrome.
  - It also misses Chrome processes started outside the launched tree, e.g. a stray `chrome --version` from the version probe. The old system-wide diff caught those, but it was flaky.
  - The testkit is for `tests/` only: inside `src/` unit tests, its `zendriver` types are a second copy.
  - `assert_eventually(within, what, || async { … })` and `assert_not_before(returned_at, event_at, what)`.
  - Gotchas learned:
    - A `fetch` with credentials to a `401 Basic` opens an auth prompt nobody answers in headless, so use `credentials: 'omit'`.
    - On Linux, Chrome rewrites its argv into one space-joined string.
    - Chrome's DevTools HTTP endpoint never answers HTTP/1.0, and it keeps HTTP/1.1 connections open (see `http_get_json` in `cdp_contract.rs`).
- [x] **0.2 OOPIF spike.** Answer: **OOPIFs are never attached, under every profile, the default included.**
  - Site isolation is on in every profile: see "Launch flags" under P1 Stealth. So `a.test` → `b.test` iframes are always out-of-process.
  - `Target.getTargets` lists the iframe target with `attached: false`, and `tab.frames()` / `frame_by_url` never see it. Same-origin subframes are registered fine.
  - Sending `Target.setAutoAttach` on the page session does make Chrome attach it. But zendriver still registers no `Frame`, for two more reasons:
    - Tabs normally run on their own per-tab socket (`tab.session().session_id()` is `None`), and that connection runs no target observers, so the `TabRegistrar` never sees the attach.
    - The parent's frames map has no entry for the remote frame, so `register_oopif_frame`'s parent lookup would fail anyway.
  - So FI #4/#5, QE #3 and TAB #5/#10 are reachable once OOPIFs get attached; the first P1 OOPIF fix is attaching them.
  - Repro: `launch_for_test(Browser::builder())`, then goto `server.url(SITE_A, "/iframe-cross-site")`.
- [x] **0.3 Port the existing repros.** From `../zd-eval/src/bin/*.rs` and `../zd-eval-mcp/t*.py`, on the local fixtures. The `stealth*` repros became 0.5; the site binaries (`github`, `youtube`, `ludios`, `hydra`, `gh*`) and most `t*.py` only print, with no assertions, so there was nothing to port. Each `known_bug_*` test fails 3 out of 3 today.
  - `crates/zendriver/tests/navigation_waits.rs` (`races.rs`): `goto` + `wait_for_load` passes; `known_bug_*` for an iframe finishing first (TAB #1), `new_tab_at`, back/forward, reload, and a stale signal from an earlier `goto` (TAB #2). Each also passed with a correct wait (a poll of the page substituted for `wait_for_load`), so they fail for the right reason.
  - Not reproduced locally: R1's Enter (and a link click) followed by `wait_for_load`, once no stale `goto` signal is left. The navigation starts before `wait_for_load` looks, even with the result document delayed 500 ms; 1 run in 5 failed, with another browser running in parallel. Test it through the planned `expect_navigation`, which subscribes before the action.
  - `zendriver_testkit::load()` takes `goto`'s load signal after polling the page, so tests that set up with it start with a clean `wait_for_load` (the review caught the Enter/click tests failing only because of that signal).
  - `crates/zendriver/tests/chrome_killed.rs` (`kill.rs`): already works, so it's a regular test. After Chrome is killed, calls fail with `Disconnected` at once and `close()` reaps the dead process.
  - `crates/zendriver/tests/browser_cookies.rs` (`cookies.rs`): tab-level jar passes; `known_bug_browser_cookies_find_by_url_and_delete`.
  - `crates/zendriver-mcp/tests/real_chrome.rs`: `known_bug_click_reaches_a_button_below_the_fold`, `known_bug_scroll_into_view_finds_an_off_screen_element`, `known_bug_cookies_get_by_url_and_delete`.
  - Learned:
    - Locally the documents arrive instantly, which hides the `new_tab_at` bug: the tab has committed before `wait_for_load` looks. `/slow-load?first_byte_ms=N` delays the document, as live sites do.
    - Back/forward restore from the bfcache fast enough to pass without waiting. Without the bfcache (`--disable-back-forward-cache`), back and reload still served the `no-store` image from a cache; `/slow-load?fresh=1` gives it a new URL per load.
- [x] **0.4 CDP contract test:** `crates/zendriver/tests/cdp_contract.rs`.
  - `COMMANDS` is every `(method, scope)` the library sends. Each row is sent to Chrome with its first parameter mistyped (types come from `/json/protocol`), so Chrome finds the method, then rejects the call without running it. Commands with no parameters are sent as `{}`, except `Browser.close`.
  - `KNOWN_MISSING` holds the two browser-scope `Network.*Cookies` calls (MEC #3). Chrome keeps rejecting them whatever the library does, so the fix has to delete those rows by hand.
  - Limitation: scopes are hand-kept, and the source scan compares only names. A new browser-scope call to a method that already has a page row isn't caught. The stronger version records the `(method, has session)` pairs actually sent during the integration suite, via a transport hook, and checks those against Chrome.
  - Quoted names whose domain isn't a CDP domain are ignored, so a misspelled domain slips through. Flagging them would need an allowlist of about 13 non-CDP names (`Math.abs`, `Chromium.app`, mock placeholders).
  - `EVENTS` is checked against the protocol.
  - A second test scans `crates/*/src`, mock tests included: any quoted CDP name without a row fails, and so does any row whose name is gone. That's the "adding an `expect_cmd` reminds you" part, enforced rather than by placement.
  - Every method and event name in the source exists in Chrome 156.
  - Scope finding: a call with no session on `session.connection()` goes to the **page** when the tab has its own socket. It's browser scope only on the shared fallback socket. See MEC #1 below.
- [x] **0.5 Stealth consistency probe.** `zendriver_testkit::{PROBE_PATH, PROBE_JS, disagreements}`; tests in `crates/zendriver/tests/stealth_probe.rs`, plus MCP `spoof_windows` in `crates/zendriver-mcp/tests/real_chrome.rs`.
  - One script reads the identity in the main world, a same-origin iframe and a dedicated worker. The page sends `Accept-CH`, so later requests carry the high-entropy `Sec-CH-UA-*` too. Served from `127.0.0.1`, a secure context (the secure-context guess was right).
  - Compares UA, OS (from UA, `navigator.platform`, `userAgentData.platform`, `Sec-CH-UA-Platform`), platform, `userAgentData` low and high entropy against their headers, `hardwareConcurrency`, `deviceMemory`, WebGL, timezone, languages vs `Accept-Language`.
  - off, default and native: everything agrees. spoofed: the worker's `hardwareConcurrency`, `deviceMemory` and WebGL differ (R6). spoofed Win32: also `os`, `uaPlatform`, `platformVersion` (ST #1).
  - The spoofed profiles pin the page's hardware to 1 CPU and 1 GB (`PINNED_HARDWARE`), so the unpatched worker disagrees on every host. Unpinned, `deviceMemory` agreed on 8 GB hosts.
  - `Accept-Language` is compared as `acceptLanguage`, with `navigator.languages` expanded as Chrome does (`en-US` → `en-US, en`).
  - The known disagreements are one table, `KNOWN_DISAGREEMENTS` in the testkit, shared with MCP's `spoof_windows` test (which leaves out the hardware fields: `browser_open` can't overlay a persona).
  - Linux-only (`cfg`): the known disagreements depend on the host OS.
- [x] **0.6 JS patch conformance test.** `crates/zendriver/tests/stealth_conformance.rs`. Nine invariants, each checked under `off()` too, so real Chrome vouches for it. Profiles: default, native, spoofed, spoofed Win32, and spoofed with a persona font list (the only way to get the `fonts.check` allow-list).
  - Every reported stealth bug in the list reproduces: ST #2, #5–#9, #11–#13.
  - This TODO first listed the invariant as `innerHeight <= documentElement.clientHeight`, which is backwards: real Chrome has `innerHeight >= documentElement.clientHeight`, since only `innerHeight` includes the scrollbar.
  - native() does inject JS (ST #12): `Function.prototype.toString` is constructible under native and default.
  - Real Chrome 156's `document.fonts.check()` is true for any local font, even one that doesn't exist. So a persona allow-list that answers false is itself a deviation from real Chrome, beyond ST #6's parsing bug.
- [x] **0.7 MCP real-Chrome tests:** `crates/zendriver-mcp/tests/real_chrome.rs`, with the driver in `tests/support/`. Stdio and HTTP (on a free port: the server now logs the address it actually bound).
  - `ChromeTree::launched_by(server_pid)` finds the server's Chrome; `McpServer` kills it on drop, so a failing test leaves no Chrome behind.
  - Pass: browse round trip over each transport, stdin close, the `spoof_windows` probe.
  - `known_bug_chrome_exits_*`: SIGKILL over either transport and SIGTERM over HTTP leave all 13 Chrome processes. SIGTERM over stdio is different: the server closes Chrome, then hangs until stdin closes. Likely cause (unverified): tokio's `stdin()` reads on a blocking thread, and runtime shutdown waits for it.
- [x] **0.8 Fix the flaky leak test.** `repeated_launch_close_leaves_no_orphan_chrome_processes` now launches through `launch_for_test`, so it checks each launch's own process tree. It also counts zombies.
  - With an unrelated headless Chrome restarting every 1.3 s, the old test reported "167 Chrome PIDs leaked"; the new one passes.
  - **Unverified on Windows**, which is the leg that runs this test. `ChromeTree` reads the browser's `--user-data-dir` from its command line via `sysinfo`, and it relies on `SystemInfo.getProcessInfo`. Watch the first Windows CI run.
- [x] **0.9 CI.**
  - The new tests aren't `#[ignore]`d, so the Linux `test-integration` job (`--workspace --features integration-tests`) gates every PR on them.
  - The nightly ignored-test lane skips `known_bug_*`, which fail by design. So no CI job runs them: one that starts failing for another reason, or starts passing because something else fixed it, goes unnoticed. An inverted nightly check (alert when one passes) would close that; not done.
  - Dropped: a `site-tests` feature. The zd-eval site binaries have no assertions to port, live sites flake, and Hydra (`finssd2:4041`) is a LAN host CI can't reach.
  - Dropped for now: an examples smoke job. Almost every example hits a live site, and `hello.rs` fails already (see Docs). Revisit with that Docs item.

---

## Handoff (2026-11-01, session 2efedb20: chunk 1)

Chunk 1 (item 1 of the planning handoff below) is done on `prime`, nothing pushed. Step 0 ran first: the five reviews of `d4cd6224b..c5b2a65bf` found four problems, fixed here. This session's work, `6f7e8b64a..fb61ded0d`, was reviewed as one batch (`~/.ai-code-reviews/{model}/7844e5102…-fb61ded0d…`), and the follow-ups `f741750ef..3ecf7f35b` weren't re-reviewed. Another session committed interception work in between (`de6816159`, `582088ee1`). Sessions share one git index: this session's `822b88f36` swept in that session's two staged renames (`interception.rs` → `sensor_signal.rs` in imperva / datadome), so HEAD didn't compile from `822b88f36` until `582088ee1`; commit with explicit paths (`git commit -- <paths>`).

Done:
- **Load tracker.** After lost events, a `goto`'s tracker asks `Page.getFrameTree` whether the main frame shows the loader id that `Page.navigate` returned, before trusting `readyState` (`6f7e8b64a`). Back, forward and reload know no destination (`f741750ef`): asking the renderer before `Page.reload` broke reloading a crashed tab, and `Page.getNavigationHistory` reports a pending history entry as the current one. `Recheck` names the states; a start after a stop, or a second start, makes the tracker wait for the next stop; a new tab's question gives way to `readyState` after its load; a failed probe leaves the state as it was; a change that arrives while the page answers goes first (`547e1e088`, `822b88f36`, `f16c0dc05`).
- **Cookies.** `for_url` matches Chrome on schemes, `localhost.` and partitions (`793fb67bd`, `a0e2a9a15`; MCP description `8ec9e3aa2`). `delete` and `set_many` handle a Secure cookie that a loopback http page set: its `NonSecure` source scheme made `Storage.setCookies` fail the whole batch (`011ba137a`, `85d2e9cf6`). The "Secure overwrite" hazard the earlier review suspected can't happen: Chrome sets each cookie of a batch from a simulated https source and ignores each result.
- **Cookie contexts** (from review). The handshake asks `Target.getBrowserContexts` for the default context, falling back to the first page's (`51159a4f9`, `28102f0d3`, `98ab2bdc3`); `Tab::cookies` keeps naming an isolated context after the Browser is gone (`847f16179`).
- **Proxy auth.** Answered challenges are remembered for 60 s in a hash set, not the last 64 (`c670e4b0e`, `84842bfb6`, from review); docs say once per request, proxy and scheme, within a minute (`b714de11e`, `d7e84a1c0`); the fixture server's proxy mode has a raw-HTTP test and `carries_basic` (`2e53df30f`).
- **Test literals:** `test_only_inner_with(conn, customize)` and `Tab::new_for_test_in` (`3f6fbd93b`, 385 lines fewer).
- **`connect`:** `BrowserError::ConnectEndpoint { endpoint, reason }`, with the status line and the start of the body, and no password or token from the endpoint; the scheme is matched in any case (`268676588`, `5dbf79036`; baseline, ledger, book).
- **MCP HTTP shutdown:** `browser_open` refuses in a session after the signal (`fb61ded0d`).

Left open:
- Not done: the terminal event on `Connection::shutdown()` (a new `AccountedRawEvent` variant); a wait still runs to its timeout after a clean shutdown.
- `goto(A)`; `goto(B)`: B's tracker can still end on A's stop when A's start and stop come after B's tracker subscribed. Checking the destination at every stop would fix it, but would break downloads (no commit) and "a navigation that began after it".
- After lost events during a back, forward or reload, the tracker waits for the next stop (a lost stop means a 30 s timeout), as before `6f7e8b64a`.
- The probe's `Runtime.evaluate` blocks while a JavaScript dialog is open (from review), so `wait_for_load` after lost events can run to its timeout then.
- **New: a crashed tab.** After `Page.crash`, the tab's session answers no renderer command, even after `Page.reload` (which returns, and in about half the runs brings the target back to its URL within 4 s; in the others its URL stays empty for over 10 s). `wait_for_load` after reloading a crashed tab times out or returns early. Chrome sends `Page.frameStartedLoading` / `frameStoppedLoading` from the browser process (`PageHandler::DidChangeFrameLoadingState`); `Inspector.targetCrashed` didn't arrive on the tab's session. Test: `reload_of_a_crashed_tab_needs_no_renderer` in `navigation_waits.rs`.
- `for_url` without the public suffix list: a cookie partitioned under a public suffix with a dot (a page at `github.io` itself) is returned for that suffix's subdomains. `psl` is in the tree (via `zendriver-datadome`) but not in the default build.
- From review, not acted on: after the shutdown signal, other MCP tools still answer "browser not open" with a hint to call `browser_open`, which then refuses (the server exits within seconds); `only_isolated_tab_jars_name_their_context` reads the jar's `Debug`; `tab_cookies_write_into_the_tabs_context` can't tell the tab's connection from the browser's; `display: contents` at `opacity: 0` may still be rejected by `checkVisibility` (Mythos, "most likely"; belongs to the honeypot round).
- `httparse` not adopted: it would replace only `complete_body`'s header split.
- MCP shutdown: a tool call holding a session's lock still delays the cleanup (at most 20 s); SIGKILL still needs the Chrome-side guard.
- Tooling: `cargo-insta` isn't installed (`INSTA_UPDATE=always` writes snapshots); `mdbook` runs through `nix shell nixpkgs#mdbook`; the public-API check needs `+nightly-2026-06-10` (the default nightly reports hundreds of unrelated items).

---

## Handoff (2026-11-01, planning session f0633309)

Nothing has landed since the 2026-10-31 handoff below: `c5b2a65bf` is still HEAD, and `TODO.md` / `ai-code-reviews/` are still untracked. The user decided the order on 2026-11-01; the items it refers to by number are the 2026-10-31 handoff's items.

0. **Review the 8 unreviewed follow-up commits first** (`d4cd6224b..c5b2a65bf`), in the background, as AGENTS.md requires: the three codex reviews with `--base d4cd6224b`, the two claude reviews with `commits d4cd6224b..c5b2a65bf`; outputs under `~/.ai-code-reviews/{model}/d4cd6224b…-c5b2a65bf…`. Fold the findings worth fixing into chunk 1.

1. **Chunk 1: finish the review follow-ups (items 2 to 7).** Suggested order:
   - **Load tracker (item 2):** remember the navigation's `loaderId` and, after lost events, compare it with `Page.getFrameTree`'s main-frame `loaderId` before trusting `document.readyState`. `crates/zendriver/src/load_tracker.rs` (412 lines; the lost-events path is `AccountedRawEvent::Lagged | Reconnected` near line 79 and `probe_complete` at line 92). The terminal event on `Connection::shutdown()` is a public-API change (a new `AccountedRawEvent` variant: baseline, ledger, docs), so do it as its own commit, or leave it.
   - **Cookies (item 3):** `for_url` (`cookies/mod.rs:438`), `delete` (`:529`), `is_secure` (`:226`), the `deleted_by` test helper (`:787`). Real-Chrome tests live in `crates/zendriver/tests/browser_cookies.rs`.
   - **Proxy auth (item 4):** `proxy_gate` (`zendriver-testkit/src/server.rs:468`) and `basic_auth` (`:306`); add a test of the proxy mode to `crates/zendriver-testkit/tests/fixtures.rs`.
   - **Test literals (item 5):** about 14 `Tab::new(..)` and 15 `BrowserInner { .. }` literals in `browser.rs` / `tab.rs` unit tests; one helper each.
   - **`connect` (item 7):** the scheme check and the `DevtoolsParse` fallbacks are at `browser.rs:3189`–`3268`. A dedicated error variant is a public-API change (baseline + ledger).
   - **MCP HTTP shutdown (item 6):** leave it unless a fix is cheap; the real fix is the SIGKILL guard decision under "P0 › MCP shutdown".
   Every fix lands with a test that fails before it, one logical change per commit, reviewed per AGENTS.md.

2. **Honeypots / `visible_only` (item 1): DECIDED 2026-11-01** by an Astra–Fable consensus round (three rounds; both signed). The full record, with the signed text, is `ai-code-reviews/honeypot-visible-only-consult/` (`decision.md`; `brief.md` has the real-Chrome probe over 20 hiding patterns and the corpus data; `survey-summary.txt` is the live-site survey: of 25 off-page rendered inputs on 239 pages, 21 were Mailchimp honeypots, and the withdrawn geometry rule would have caught 12 and exempted 13 through an `overflow: hidden` body or wrapper, while the 4 non-Mailchimp ones were legitimate off-canvas search boxes). Implement `decision.md` as written; in short:
   - The filter and `is_visible` stay "rendered": no geometry, no level enum, no `is_on_page`. Rust default `visible_only(false)`, MCP default `true`.
   - New `Element::is_in_viewport()` (= `check_visible(Within::Viewport)`, no scroll), filling `browser_read.in_viewport` for the `all` / `geometry` / `visible_enabled` presets.
   - New `Element::wait_until_in_viewport_and_enabled(timeout)`: `scroll_into_view` + the TEXT_INPUT poll, no focus; `focus()` = that + focus. Library `set_value` / `clear` / `set_text` stay ungated, with rustdocs pointing at the new method as the opt-in gate. MCP `browser_set_value` (both modes), `browser_clear` (value mode) and `browser_type` with `clear_first` run it first unless `unchecked: true`; backspace-mode clear rejects `unchecked`.
   - `wait_actionable` re-scrolls before the next poll only after a failed visible check; no fast-fail.
   - `NotActionable` becomes a struct `{ timeout, reason, bbox, viewport }` with `NotActionableReason::{NotRendered, OutsideViewport, Disabled, Unstable, HitTestMissed { at, hit }}` and the display strings in `decision.md`. Breaking: regenerate the public-API baseline.
   - MCP on `NotActionable`: one re-query (same selector / frame / `visible_only`, no `nth`, timeout 0, limit 10); the count, up to two other matches by node identity as bounded descriptors, and the suggestion to narrow the selector or list matches with `browser_find_all` before choosing another `nth`. The `browser_html` hint goes.
   - Action tools: optional `in_aria_hidden_subtree: true` (omitted when false); always present in the failure descriptors.
   - Documented limits: clipped and far-below honeypots remain actionable.
   - With the implementation commit: a new entry in `docs/api-audit/decisions.md` superseding #1's fork note, and the CLAUDE.md checklist (ledger, baseline, snapshots, README, book). Real-Chrome tests from the probe's fixture page: a honeypot before the real input, per hiding pattern (`honeypot_probe.rs` in the record folder).

3. **Chunk 2: the interception P0 cluster** — done 2026-11-06 (`fde097f40`..`c4b58b531`, see "P0 › Interception"); reviews of `222fe294b..907f3a7f2` under `~/.ai-code-reviews/`. Its layer model was then replaced by decision #15 (`docs/api-audit/decisions.md`, design in `docs/api-audit/interception-precedence.md`): one ordered list of entries per tab, every insertion at an explicit `Position`, the first fitting entry decides, `browser.trackers` an ordinary entry (`b357bdb23`, `0a3dc51e5`..`777c35159`, MCP `4dd5e05e0`; reviews of `b965a0d09..777c35159`, `4dd5e05e0` and `b357bdb23` under `~/.ai-code-reviews/`). The original plan: the six open items under "P0 › Interception". Today each owner starts its own actor and its own `Fetch.enable`: `browser.rs:3762` (main tab: proxy auth + tracker blocking), `browser.rs:2375` (new tabs: tracker blocking + per-context proxy auth), `tab.rs:3140` (`Tab::intercept()`: user rules, and MCP's one-actor-per-rule in `zendriver-mcp/src/tools/intercept.rs`), and `zendriver-imperva` / `zendriver-datadome` `src/interception.rs`. The actor's enable path is `run_actor` in `zendriver-interception/src/actor.rs` (line 282; `Fetch.enable` is fire-and-forget at line 314). Ask the user before settling the multiplexer's shape (AGENTS.md: ask when there are several good designs for state).

4. **Then** `expect_navigation` / `wait_for_url` (P0, R1), which builds on the load tracker after chunk 1's `loaderId` fix.

---

## Handoff (2026-10-31, end of session a5fcde83)

Landed on `prime` this session (`a3338f6e4`..`c5b2a65bf`), nothing pushed: tab cookie jars per context, visibility semantics, proxy auth only to the proxy, load-tracker lost-events handling, plus follow-ups from two five-reviewer batches (`2b4ad50e9..3360530c9` and `3360530c9..d4cd6224b`; outputs in `~/.ai-code-reviews/`). Follow-up commits weren't re-reviewed. Earlier session's work: `8f0e12424`..`2b4ad50e9`. Start here:

1. **Decide (ask the user): honeypots and `visible_only`.** `visible_only` / `is_visible` now mean "rendered" with no position test (`3360530c9`; upstream decision #1 superseded in `decisions.md`). Two Claude reviewers objected: `left: -9999px` honeypots now pass the filter, which MCP's selector tools apply by default; `browser_click` then fails at the gate after 5 s instead of picking the real element, and `browser_set_value` (no gate) fills a honeypot input. A second consult split:
   - Astra: add a deliberately conservative "before the page's start" test: reject a box entirely above the document top (`rect.bottom + scrollY <= 0`) or, on an LTR document, left of its left edge, only for principal `horizontal-tb` flow, and only when no ancestor is a scroll container (`overflow` auto/scroll/hidden/overlay; count `body` when it isn't `document.scrollingElement`); merge into the opacity walk; document the `position: fixed` misses. Tests: independent body scrolling, vertical writing, reversed nested scroller, a honeypot before the real input.
   - Fable: no geometry in the filter (`html, body { overflow-x: hidden }`, common on exactly the sites with honeypots, exempts everything; the rules keep growing). Instead gate MCP's `browser_set_value` / `browser_clear` (scroll + `TEXT_INPUT` gate, maybe an `unchecked` input), and put the bounding box in MCP's NotActionable error. Optionally add `receives_pointer` to `TEXT_INPUT` (false "occluded" on floating labels).
   - Also from review: `is_visible` no longer answers "on screen now", and nothing public does; MCP `browser_read`'s `in_viewport` is still always null. An `Element::is_in_viewport()` (= `check_visible(Within::Viewport)`) would restore it and fill that field (public API: baseline + ledger + docs).
2. **Load tracker: lost events.** After lost events (bus lag or reconnect), a navigation's tracker that hasn't seen its stop waits for the next stop (`d4cd6224b`, `6d88cb1a8`), so if its stop was among the lost events, `wait_for_load` times out after 30 s on a loaded page; and a `goto(A)`; `goto(B)` race can still end on A's stop. Both reviewers suggested the real fix: remember the navigation's `loaderId` (`Page.navigate` returns it; for back/forward/reload, the frame's `loaderId` before the navigation), and on lost events compare `Page.getFrameTree`'s main-frame `loaderId` before trusting `document.readyState`.
   - A clean `Connection::shutdown()` (e.g. `Tab::close`, dropping a `connect`ed `Browser`) emits nothing on the accounted bus, so a wait runs to its timeout; docs now say so. Fix in the transport: `shutdown()` is a separate entry point from `reconnect()` and could emit a terminal event (a new `AccountedRawEvent` variant is a public-API change; the monitor and other accounted consumers would benefit too).
   - After `Connection::reconnect`, a flat-session tracker's session id is stale, so it can only time out (a per-tab socket survives a reconnect).
   - Cost: every tracker subscribes to the accounted bus (turning on the actor's per-event clone while one is parked); `probe_complete` sends `Page.enable` every time.
3. **Cookies, still open from review:** `for_url` ignores URL schemes (an `ftp://` or `file://` URL gets the host's cookies; Chrome returns none); `localhost.` (trailing dot) isn't treated as secure; partitioned cookies of every partition are returned; one expired copy that Chrome rejects (Secure overwrite rules) fails the whole `delete` batch; the `deleted_by` test helper waits only 200 ms for the `setCookies` it then answers; `is_secure` sits below its caller. `Tab::set_download_path` / `expect_download` had the same context problem the cookies had (fixed 2026-11-06, see "Download directory tracked per tab").
4. **Proxy auth, smaller:** the fixture server's proxy mode has no test of its own in `zendriver-testkit` (only `proxy_auth.rs` exercises it), and `proxy_gate` repeats `basic_auth`'s credential check (a shared `carries_basic(headers, header, user, pass)` would do). A site's own HTTP auth now has no helper (docs point to `modify_request` with an `Authorization` header).
5. **Visibility, smaller:** the opacity walk can't see a closed shadow root's slots (`assignedSlot` is null from page script), so a faint wrapper there is skipped (documented). ~13 test-only `Tab::new(...)` / `BrowserInner { .. }` literals grow with every new field; a test helper would shrink them.
6. **MCP HTTP shutdown, from review:** a tool call holding a session's lock delays its cleanup (bounded at 20 s now), and the session tasks keep serving after the signal, so a `browser_open` after the snapshot isn't closed. The real fix for both, and for SIGKILL, is the Chrome-side guard under "MCP shutdown" below.
7. **`connect`:** a non-200 reply or a bad body still becomes `DevtoolsParse` with the status and body dropped (a dedicated error variant is now feasible: `cargo-public-api` is installed, see Local setup); the scheme is chosen with case-sensitive `starts_with` before the case-insensitive URL parse; `httparse` is in the tree (via tungstenite) and could replace the hand-rolled framing.
8. **Next P0/P1 candidates looked at:** "Errors in a query look like not found" (QE #4, R7): `Runtime.evaluate` reports a thrown `SyntaxError` as `exceptionDetails` in a successful reply, and `extract_array_refs` (`query/selectors.rs`) reads the thrown Error object's properties, finds no numeric ones, and returns an empty list, so the poll loop in `FindBuilder::one` / `many` retries until the timeout. Fix: check `exceptionDetails` in the resolvers (css, xpath, predicate, text regex) and return the error at once.

---

## P0: security, hangs, and features that never work

### Interception
- [x] **Proxy credentials sent to any server's 401.** *(TI #1, #8)* Done 2026-10-31 (`cff8e760f`, `748b6f620`, `cc1f7d2f3`): only `authChallenge.source == "Proxy"` gets them, once per request, challenger and scheme (then `CancelAuth`; answers are remembered for 60 s since `c670e4b0e`, the last 64 before); a server gets `Default` (headless then fails the navigation with `ERR_INVALID_AUTH_CREDENTIALS`, as it does without interception). `handle_auth` renamed `handle_proxy_auth`. The fixture server doubles as an authenticating proxy (`proxy_url()`, `PROXY_USER` / `PROXY_PASSWORD`); tests in `crates/zendriver/tests/proxy_auth.rs`.
- [x] **One `Fetch` owner per session.** *(TI #2, MCP #2, FW #1)* Done 2026-11-06 (`e8e6e5aa7`). The shape was chosen by the user after an Astra–Fable consult (two rounds; transcript in `/tmp/mux-consult/` while it lasts): a `zendriver_interception::Interceptor` per session, held by `TabInner` (`InterceptBuilder::new(&Interceptor)`, no registry, no transport change); newest layer first. Each `start()` / `subscribe()` adds a layer (rules or a stream, its patterns, optional proxy credentials) to the session's multiplexer task, which sends `Fetch.enable` with the union of the layers' patterns or `Fetch.disable` when none is left; a pause goes to the newest layer whose pattern matches its URL, resource type and stage and that has a rule for it (a stream layer takes all it considers). The browser's tracker blocking + proxy credentials are each tab's oldest layer, started on the tab's own session before the target runs, by one function (`BrowserInner::start_browser_intercept`). MCP: one layer per rule. *Since `0b7d285c2` (decision #15) the layers are gone: one ordered list of entries per tab, each inserted at an explicit `Position`, the first fitting entry decides; `browser.trackers` is the tab's first entry and the proxy credentials are one per-tab setting outside the list.* Tests: `crates/zendriver/tests/interception.rs` (`a_rule_and_the_proxy_credentials_compose`), `crates/zendriver-mcp/tests/interception_rules.rs`.
- [x] **`start()` / `subscribe()` return before Chrome has acknowledged `Fetch.enable`.** *(TI #3)* Done 2026-11-06 (`e8e6e5aa7`): both are `async` and return after the reply; the 150 ms sleeps are gone. Test: `a_request_right_after_start_is_intercepted` (a `goto` right after the old `start()` usually won the race anyway; a `fetch` didn't).
- [x] **Requests that never get released.** *(TI #5, #6, #10)* Done 2026-11-06: a panicking closure (`catch_unwind`) and a dropped stream (its unread pauses are continued, its layer removed) in `e8e6e5aa7`; a release Chrome refuses (falls back to `continueRequest`), an event that doesn't decode (continued by its `requestId`), and `PausedRequest` released only after Chrome accepted, in `6c36b6e03`. *Since `777c35159` (decision #15, rule 10) each of those fails the request (`Fetch.failRequest`, `Failed`, logged with the entry's id) instead of continuing it; an event that doesn't decode is still continued.* Test: `a_paused_request_left_unanswered_fails`.
- [x] **Stages ignored when matching rules.** *(TI #4, #11, MCP #3)* Done 2026-11-06 (`2c8627260`): a rule that can't act at the stage is skipped (redirect / modify_request only before the response, modify_response only after); without explicit patterns a layer's patterns come from its rules (each rule's URL pattern at its stage; `*` at the Request stage for `block_hosts` and proxy credentials). Tests: `rules_pause_at_their_own_stage`, MCP `a_modify_response_rule_changes_the_status`.
- [x] **`BrowserBuilder::proxy_auth` reached only the main tab.** *(found 2026-11-01 by Fable; the user chose to include it)* Done 2026-11-06 (`bc8a71f74`): every default-context tab gets the credentials, and `connect()` honors them. Test: `proxy_auth.rs` `a_later_tab_gets_the_proxy_credentials`. Still open: an isolated context without a proxy of its own uses the browser's proxy but gets none of `proxy_auth`'s credentials (giving them would need the context builder to record whether the context has its own proxy).
- [x] **`Browser::reconnect` left the old tabs' own sockets open**, so their interception kept answering the target's requests beside the rebuilt tab's. *(found by both consultants)* Done 2026-11-06 (`907f3a7f2`). Test: `crates/zendriver/tests/reconnect.rs`.
- [x] **`Fetch.continueResponse` sent with only status or only headers.** *(TI #7)* Done 2026-11-01 (`de6816159`): an override of the status, the headers or the phrase is completed from the paused response (the upstream phrase only with the upstream status; Chrome refuses an empty phrase and a code it has no phrase for, e.g. 299). Test: `crates/zendriver/tests/interception.rs`.
- [x] **Imperva/DataDome `with_interception()` leaves `Fetch` on.** *(FW #1)* Done 2026-11-01 (`582088ee1`), by the user's choice: the fast path watches `Network.responseReceived` instead of pausing through `Fetch`, so the bypasses are no longer `Fetch` owners (both crates dropped `zendriver-interception`). Test: `crates/zendriver/tests/waf_fast_path.rs`. The `imperva` / `datadome` features of `zendriver` still imply `interception`, which they no longer need.
- [ ] **Interception: review findings left open** *(reviews of `222fe294b..907f3a7f2`, 2026-11-06; fixed ones are `662bb63c4`..`90ca6bb78`)*
  - ~~**Does tracker blocking yield to user rules?**~~ Settled by decision #15: every insertion names its `Position`, and `browser.trackers` is an ordinary entry.
  - Lost events (`Lagged`): the recovery's `Fetch.disable` continues every paused request, those a block rule or a stream would have held included, and the backlog read after it holds pauses already continued (their releases then fail, with a warning). Lag on the accounted bus counts every event on the connection, not just this session's `Fetch` ones; the multiplexer stops reading while it waits on Chrome (`apply`, `recover`). Running those round trips beside the event loop would make lag rarer.
  - A stream layer's queue is unbounded (Chrome holds the requests anyway); the auth memory doesn't know which credentials answered, so new credentials for the same request and proxy within 60 s get `CancelAuth`.
  - Each intercepting tab subscribes to the accounted bus, which turns on the transport's per-event clone for it.
  - `send` / `release` in `interceptor.rs` share a shape. (`Rule::pattern` is gone.)
  - After a lost-events recovery, a stream consumer's late answer to a pause that `Fetch.disable` already continued is refused, and the follow-up `failRequest` is refused too: two log lines per such pause.
  - *Open from the reviews of `b965a0d09..777c35159`, `4dd5e05e0` and `b357bdb23` (2026-11-06; fixed in `4f999903a`..`96a81fb20`):*
    - An entry inserted while a request is already under way (paused at the Request stage, or sent before `insert` returned) doesn't get that request's Response pause: Chrome fixed its stages when it started. Documented as "a request sent from then on". Sending `interceptResponse` on every Request-stage continue, worked out (by resource type, as for a new URL since `94768fd1e`) from the list at answer time, would close it.
    - When Chrome refuses both an answer and the `failRequest` that follows it, the hold is dropped as answered, so a later `Fetch.disable` may release the request (if Chrome still had it). Keeping the hold would keep `Fetch` on for a request Chrome most likely no longer knows. *(Astra, `b965a0d09..777c35159`; its other findings are fixed in `94768fd1e`..`dc9111573`.)*
    - A `Fetch` pattern also pauses prefetches, which the multiplexer reads as `Other`, so a `Fetch` entry doesn't fit them and they're continued unchanged.
    - Registrar: two attaches of one target handled at once can still build two tabs; under the flat-session fallback, moving the registry key to the new session leaves the kept `Tab` on its old session, and `on_target_detached` removes by key.
    - A new tab with both proxy credentials and tracker blocking makes two `Fetch.enable` round trips before it runs.
    - MCP: `origin` is worked out by elimination (an entry not in `SessionState::rules` is `browser`); an owner tag on the core `Entry` would be firmer. `list_rules` shows the current tab only, while `remove_rule` / `clear_rules` reach every tab. `add_rule` returns an error if the listing after the insert fails (the rule stays in, its id unreported). `clear_rules` stops the handles one by one (a round trip each). `ResourceTypeName` / `PauseName` mirror core enums by hand. `hosts` isn't IDN-normalized; `respond` takes any status code; `modify_request` takes forbidden header names.
    - `docs/api-audit/interception-precedence.md` is the design record as agreed, not the API as built: its signatures leave out `Entry` / `UrlInfo` / `ActionKind`, and its `remove(tab.tracker_blocking())` needs an unwrap.
  - MCP: `browser_tab_close` drops the rules of the tab it closes, but a tab that closes in another way (the page's `window.close()`, a crash) leaves its rules in `SessionState::rules` until `browser_intercept_clear_rules` or `browser_close`. They do nothing (their tab's list is gone), but `browser_intercept_remove_rule` still accepts their ids.
- [ ] **Interception misses out-of-process iframes.** *(found 2026-11-01, reading Chromium)* A frame's requests go through the `Fetch` handlers of that frame's own agent host plus every browser target's (`content/browser/devtools/devtools_instrumentation.cc:2041`), so `Fetch` on a page session doesn't see an OOPIF's requests: tracker blocking, proxy auth and user rules all miss third-party iframes. Fix candidate: give each attached `iframe` session (the registrar's `iframe` arm) its own multiplexer with the library's layer. Test `chrome`: a cross-site iframe (`/iframe-cross-site`, with site isolation) whose tracker request is blocked.

### Navigation waiting (biggest real-world pain: R1)
- [x] **`wait_for_load()` listens to the wrong events** (TAB #1, TAB #2, MCP #10), **`new_tab_at(url)` + `wait_for_load()`** (R1), **`back()` / `forward()` / `reload()`** (R1, MCP #4), **`Frame::goto`** (FI #2). Done 2026-10-31 (`crates/zendriver/src/load_tracker.rs`; tests in `navigation_waits.rs`, MCP `history_tools_wait_for_the_page_they_reach`).
  - A `LoadTracker` follows the main frame's `frameStartedLoading` / `frameStoppedLoading`, subscribed before the navigation. Chrome 156 sends that pair for every navigation kind (cross/same-document, bfcache restore, reload, download, error page); a navigation that replaces a load sends a second start, no stop. An armed tracker ignores stops until a start; lag or reconnect re-probes `readyState`; a dead socket ends the wait at once.
  - `back` / `forward` / `reload` work like `goto` (they arm the next `wait_for_load`); chosen after comparing other libraries, as the user asked: Playwright, Puppeteer, Selenium and chromedp wait inside all of goto/back/reload, zendriver-py waits on nothing meaningful, and zendriver-rs's separate waits play the part of Playwright's `waitUntil`. Making all four wait (Playwright-style) would be a separate, bigger change. MCP's history tools take `wait_for` like `browser_goto`.
  - `new_tab_at` can't be blank + `goto`: that makes `history.length` 2 instead of 1.
  - Left open, found on the way:
    - `back()` to a history entry where only an iframe navigated: the main frame never loads, so `wait_for_load` times out after 30 s (MCP `browser_back` too, unless `wait_for: none`).
    - `new_tab_at` takes as long as the new page keeps its main thread busy (a 2 s busy loop, even one scheduled with `setTimeout(0)`, made it take 2 s), with or without the load tracker: something in the attach chain waits on the renderer.
    - `wait_for_ready_state` right after `goto` can read the old document: `Page.navigate` replies before the commit.
    - Cost: an armed tracker is a task filtering every event on its connection until the next wait or navigation; on the flat-session fallback that's tabs × events. The reviewers suggested one tracker per tab instead, created in the registrar before the target resumes; it needs a way to order the caller's arming against events in flight (e.g. the accounted stream's sequence numbers).
- [x] **No way to wait for a navigation or URL.** Done 2026-11-06: `Tab::expect_navigation()` (armed before the action; resolves after the next main-frame navigation has committed and loaded, at once for a same-document one) and `Tab::wait_for_url(pattern)` (a state: ends at once if already there, skips the pages on the way), both `NavigationExpectation` futures with `.timeout()` resolving to `Navigation { url, same_document }`. MCP: `browser_wait_for_url`, and `wait_for_navigation` on `browser_click` / `browser_press`. Tests: `navigation_waits.rs`, MCP `real_chrome.rs`. *(R1)* New public API, so follow the CLAUDE.md checklist: MCP tool or ledger entry, rustdoc, README, mdBook.
- Tests for `expect_navigation`: submitting a form with Enter; `goto` → `wait_for_idle` → click a link → wait.

### Broken outright
- [x] **`Browser::connect("http://…")` always fails** *(BR #1)*. Done 2026-10-31: HTTP/1.1, body read by `Content-Length`, URL-parsed (IPv6 brackets), `https://` rejected; tests `connect.rs` and unit tests in `browser.rs`.
  - Done 2026-11-01 (`268676588`): a bad endpoint, `https://`, a failed TCP connect, or a `/json/version` reply without a `webSocketDebuggerUrl` return `BrowserError::ConnectEndpoint { endpoint, reason }`, with the status line and the start of the body; the scheme is matched in any case.
- [x] **Browser-level `cookies().for_url()` / `.delete()` fail** *(R5, MEC #3, #8)*. Done 2026-10-31 for the browser jar; the regression it caused in tab jars of isolated contexts was fixed in `a3338f6e4` / `78acbbaa0` / `6c32eb436` (a default-context jar names no context: Chrome before ~153 rejects the default context's id). Changes: both scopes through `Storage.*`: `for_url` filters `Storage.getCookies` by RFC 6265 domain/path/`Secure` (a real-Chrome test checks it against `Network.getCookies` over 40 URLs); `delete` overwrites matches with expired copies, so `delete(name, None, None)` deletes the name everywhere; `domain` ignores a leading dot.
- [x] **MCP selector tools can't see off-screen elements** *(MCP #1)*. Done 2026-10-31; semantics settled in `3360530c9`, with the honeypot question still open (Handoff 1). Defaulting MCP's `visible_only` to false would pick `display: none` duplicates.
- [ ] **MCP shutdown leaves Chrome running.** SIGTERM is done 2026-10-31 over both transports (`chrome_exits_on_sigterm_over_*`): stdio no longer waits for its stdin thread at exit, and HTTP closes every session's browser; since `fb61ded0d`, `browser_open` refuses in a session after the signal. *(R, MCP #6)*
  - Left: SIGKILL (`known_bug_chrome_exits_on_sigkill_over_*`) needs a guard on Chrome's side. Linux `PR_SET_PDEATHSIG` in a `pre_exec` is the simplest, but it fires when the *thread* that spawned Chrome exits, not the process: a launch from a `spawn_blocking` thread (which exits after 10 s idle) or a short-lived thread would kill Chrome early. A watchdog (a pipe Chrome's helper reads, or a tiny reaper process) avoids that. Either way it's a library change for every user, so decide whether it's opt-in.

---

## P1: wrong results

### Frames and elements
- [ ] **`Frame::evaluate_main` / `content` run in the top-level page for same-origin iframes.** Pass the frame's main-world `contextId`. *(FI #1)* Test `chrome`.
- [ ] **Isolated-world context id cached forever.** Invalidate it on `executionContextsCleared` / `Destroyed` for frame queries and `Element::evaluate`. *(QE #1)*
  - Test `chrome`: find in a frame, navigate the frame, find again; `Element::evaluate` after a navigation.
- [ ] **Stale-handle refresh of a frame element re-runs the selector on the top-level page.** Re-resolve inside the frame instead. *(QE #2)* Test `chrome`.
- [ ] **`best_match` handles refresh to a different element.** *(QE #9)* Test `chrome`.
- [ ] **OOPIF fixes.** 0.2 found that OOPIFs are never attached and that every profile makes them, so all of these are reachable:
  - per-page-session auto-attach. Send it on the session that will deliver the `attachedToTarget` to the `TabRegistrar`: the per-tab socket runs no observers.
  - register a parent frame entry for the remote frame. Today the parent's frames map has none, so `register_oopif_frame` can't find a parent.
  - elements bound to the OOPIF's session *(QE #3)*
  - parent id *(FI #4)*
  - `frameDetached` with reason `swap` *(FI #5)*
  - in-flight request leak in the idle tracker *(TAB #5)*
  - `force_open_shadow_roots` *(TAB #10)*

  Test `chrome` with the cross-site fixture.
- [ ] **`set_value` / `clear` throw on `<select>` and non-input elements.** Pick the setter that matches the element's own prototype. *(QE #5)* Test `chrome`.
- [ ] **`BoundingBox` mixes the content-box origin with the border-box size.** *(QE #6)*
  - Test `chrome`: padded+bordered button; `elementFromPoint` at the computed centre hits it.
- [ ] **`check_stable` waits on `requestAnimationFrame` with no timeout.** Bound the wait. *(QE #10)* Test `chrome` with a background tab.

### Selectors
- [ ] **Text selectors pick the wrong element.**
  - `text_exact()` returns the outermost match (a table cell, not the link inside), so the click lands on nothing yet reports success. *(R2)*
  - Text selectors also match `<title>`, `<script>` and `<style>`. *(QE #7)*
  - Prefer the deepest rendered match. Test `chrome`: table-with-link fixture.
- [ ] **Role selector ignores implied ARIA roles.** It found 0 of 32 ordinary links (`a[href]` should be `link`; same for buttons, headings, and so on). *(R3)*
  - Test `unit` for the role mapping table, `chrome` for the link fixture.
- [x] **Errors in a query look like "not found".** An invalid CSS selector or bad regex waited out the full timeout. Fixed in `32e22b06e`: a query whose script throws fails with `JsException` on its first poll (`tests/query_errors.rs`). *(QE #4, R7)*
- [x] **`(?i)` isn't valid JS regex syntax.** The docs now recommend `(?i:…)` instead (`9e398a1dc`, with a `chrome` test); a pattern that JS can't parse fails at once (`32e22b06e`), and MCP no longer pre-checks `text_regex` with Rust's `regex` crate. *(QE #8, MCP #8)*

### Downloads, expect, monitor
- [x] **Download directory tracked per tab.** Done 2026-11-06: one writer per browser (`download_behavior.rs`, `7a1fdf165`), on the browser's connection, per browser context, always with `eventsEnabled`; `expect_download` uses it too. Test: `crates/zendriver/tests/downloads.rs`. *(MEC #1, TAB #3)*
- [x] **Download progress race.** Done 2026-11-06: one record per guid, made by whichever of the tab's `Page.downloadWillBegin` and the browser's `Browser.downloadProgress` comes first, and kept for the last 256 finished downloads. Test: `a_tiny_download_is_found_however_the_events_race` (20 rounds). *(MEC #2)*
- [ ] **Downloads, phase 2** (phase 1, 2026-11-06: downloads followed at browser scope, so a `MatchedDownload` outlives its tab; designed with Astra and an Opus critique):
  - `Browser::expect_download` (or popup attribution): a download in a tab that Chrome opened for it (a `target=_blank` link to a file) matches no `expect_download` today. Playwright attributes a download of a page that hasn't loaded its first document to its opener.
  - MCP download ids: a matched download kept in the session, with status / save / cancel / release tools; today `browser_expect_register { save_to }` is the only way to the file, and without it the handle is dropped.
  - `MatchedDownload::cancel` (`Browser.cancelDownload { guid, browserContextId }`).
  - Deleting a download's file when its last handle drops: only after the download ends (Chrome writes `<guid>.crdownload` and renames it at the end), so a crawler with a tab per download doesn't fill the disk until the `Browser` drops.
  - `connect()`: an "observe" mode (`behavior: "default"` with `eventsEnabled`, the file found by `filePath`) might touch a person's Chrome less than sending its downloads into the staging directory.
  - If Chrome drops the deprecated `Page.downloadWillBegin`, attribute by `Browser.downloadWillBegin`'s `frameId` instead (main frame = the tab's target id; subframes need a reliable frame index, which today's OOPIF registry isn't).
  - `BrowserContext::set_download_path`, and `set_download_path` exporting completed files by copy (Astra's design) if `filePath` (Chrome 138+) turns out not to be enough.
- [ ] **`expect_response(...).body()` reads before the body has arrived.** Wait for `loadingFinished`. *(R4, MEC #7)* Test `chrome` with the trickled-body fixture.
- [ ] **File-chooser intercept left on after an error.** Disable it on the error path. *(MEC #4)* Test `chrome`.
- [ ] **Monitor `Lagged` clears the URL map.** *(MEC #5)* `stream_bodies` chunks can also arrive out of order. *(MEC #6)* Tests `chrome` (or `unit` with a forced small buffer).
- [ ] **From the 2026-11-06 reviews of the detach work (older problems, not fixed):**
  - A monitor's body-streaming tasks hold its output channel, so chunks can follow its terminal `Shutdown` / `Disconnected` boundary, and the stream doesn't return `None` until they finish.
  - Interception: dropping the last `Interceptor` and rule handles closes the command queue while an answer task (which holds only a weak sender) is pending, so `Mux::close` disables `Fetch` before a rejected `Fetch.fulfillRequest`'s fallback `Fetch.failRequest` runs (reproduced with `MockConnection` by a reviewer).
  - Simplification: in `load_tracker.rs` and `navigation.rs`, `changes` ends at `Disconnected` / `Shutdown` with `take_while`, so the `filter_map` arms for those variants are dead; the two `changes` adapters are near-copies.
- [ ] **`get_all_urls(true)` fails on SVG `href`.** *(TAB #4)* Test `chrome` with the SVG sprite fixture.
- [ ] **A wait that starts after its flat session detached runs until its timeout.** `SessionHandle::subscribe_accounted` ends at the session's `Target.detachedFromTarget` (`11565240d`), but a subscription made after the detach never sees it, so on a tab without its own socket, an `expect_*`, a navigation wait, a Strict `wait_for_idle`, or a monitor started after the tab closed (e.g. on a clone of the tab, after `tab.close()`) runs until its timeout; a monitor never ends. A tab with its own socket is fine: the connection replays its end to a late subscriber. Fixing it needs the transport to remember detached sessions (bounded, e.g. only while a `SessionHandle` for the session lives); decided on 2026-11-06 to leave it until it bites.
  - Same root cause, found in review: a stream that loses the detach in a `Lagged` gap never ends either. The expect waits and Strict idle end on the `Lagged` anyway, but a monitor keeps going, and the interceptor's `recover()` gets "session not found" from `Fetch.disable` / `Fetch.enable`, logs it, and keeps its entries, so `interceptor.subscribe(...)` streams never end. On the shared socket every tab's traffic fills each session's lag window, which makes this likelier (and a `Lagged` caused only by other tabs' events makes the interceptor release this tab's held pauses).
  - Unverified: an OOPIF's session may be dropped with its page's session without a detach of its own, so its waits wouldn't end.

### Browser lifecycle
- [x] **Main tab attached twice at launch.** *(BR #2)* Done 2026-11-07 (8fd829948): `finish_connect` no longer sends `Target.attachToTarget`; it takes the session that auto-attach opened, once the observer chain on it reached the registrar (so `launch`/`connect` also wait for the main tab's observers; `TabNotFound` after 20 s). Tests: `observers_run_once_on_the_main_target_before_launch_returns` in `tab_registry.rs`, and two `finish_connect_*` unit tests; `stealth_conformance.rs` no longer pins the seed (915e0001c).
- [ ] **`connect()` patches pre-existing tabs (contrary to its docs) and builds the fingerprint from an empty executable path.** *(BR #3, #4)* Test `chrome`.
- [ ] **`new_tab_at` with `chrome://` / `chrome-extension://` URLs stalls and returns `TabNotFound`.** *(BR #5)* Test `chrome`.
- [ ] **`geo_auto()` captures the proxy at call time.** Calling `.geo_auto().proxy(..)` in that order probes from the real IP. *(BR #6)* Test `unit` on builder order.

### Transport and input
- [ ] **A reply frame that fails to parse hangs its caller for 180 s.** Fail the pending caller instead. *(TI #9)* Test `unit`.
- [ ] **Grapheme clusters longer than 4 UTF-16 units fail.** Split into insertText or per-codepoint events (ZWJ emoji, flags, skin tones). *(FI #3)*
  - Test `chrome`: `type_text("👩‍💻🇯🇵👍🏽")` round-trips.

### Stealth
These are mostly covered by `probe` (0.5) / conformance (0.6), plus `unit` where noted. A fix deletes the bug's row from `KNOWN_DISAGREEMENTS` in `zendriver-testkit`'s `probe.rs` (shared by `stealth_probe.rs` and MCP's `real_chrome.rs`) or `KNOWN_BUGS` in `stealth_conformance.rs`.
- [ ] **Launch flags: site isolation is never disabled, and one `--disable-features` is lost.** Found in 0.2 (Chrome 156).
  - `--disable-features=site-per-process` does nothing; the feature is called `SitePerProcess`. `--disable-site-isolation-trials` also works.
  - When `--disable-features` appears twice, Chrome keeps only the last. `build_flags` emits `PasswordManagerOnboarding,AutofillServerCommunication` and then the stealth line, so the first is dropped. The comment there says Chrome takes the union; it doesn't.
  - Merge every `--disable-features` (and `--enable-features`) into one argument.
  - Decide whether site isolation should really be off. The profile docs say it is, but turning it off now changes behaviour for every user: cross-site iframes would stop being OOPIFs.
  - Test `chrome`: count the `iframe` targets on `/iframe-cross-site`, as `cross_site_iframe_is_out_of_process_under_site_isolation` in the testkit's `fixtures.rs` does.
- [ ] **JS-visible and HTTP identity come from different sources.** One identity source should feed JS, the UA, UA-CH and headers. *(ST #1, R6)*
  - Under `spoof_windows` on Linux: UA says Windows but `navigator.platform` is Linux; page and worker disagree on platform; OS version is a Linux-style `5.15.0`; WebGL names Mesa.
- [ ] **Worker GPU name and `deviceMemory` differ from the page under `spoofed`.** *(R6)*
- [ ] **Canvas farbling turns transparent pixels into `(1,1,1,0)`.** *(ST #2)*
- [ ] **macOS UA-CH `platformVersion` / `architecture` describe an impossible machine.** *(ST #3)*
- [ ] **`chrome_version(major)` invents a build number.** *(ST #4)* Test `unit`.
- [ ] **`fonts.js` bugs:** `measureText` isn't deterministic; `fonts.check` keeps only the last word of the family name. *(ST #5, #6)*
- [ ] **`innerHeight` is smaller than `clientHeight`.** *(ST #7)*
- [ ] **`permissions.js` returns `'default'`.** It should be `'prompt'`. *(ST #8, R6)*
- [ ] **`plugins.js`:** needs 5 entries and a real `PluginArray`. *(ST #9, R6)*
- [ ] **`userAgentData`:** the object is rebuilt on every access, and `getHighEntropyValues` ignores the requested hints. *(ST #10)*
- [ ] **`getClientRects()` returns an `Array`,** not a `DOMRectList`. *(ST #11)*
- [ ] **Masked functions are constructible** in the spoofed patches: each is an ordinary `function`, with an own `prototype` (rebrowser's `nativeTampering` checks for one). *(ST #12)*
  - `native()` no longer injects JS at all: under headless, it gives Chrome a screen with `--screen-info` and `--start-maximized` instead of `Emulation.setDeviceMetricsOverride` plus `screen.js`. That also fixed ST #7 and ST #13 for `native()`.
- [ ] **Own properties on instances** where Chrome has them on the prototype. *(ST #13)*
- [ ] **`broken_image.js`** reports 16 for every image with no `src`. *(ST #14)*
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
- [ ] **`browser_tab_close` leaves some of the tab's work running.** *(reviews of `c4b58b531`, 2026-11-06; several reproduced against real Chrome by gpt-6-astra)* Monitors (`MonitorState` has no `tab_id`; `monitor::start` has the tab, so storing it is cheap); an expectation that `browser_expect_await` already removed from the registry; an expectation registered concurrently (the lock is released between resolving the tab and inserting); a tab that closed by itself before the call (`TabNotFound` returns before `drain_tab_scoped`); and the `expect_dialog` / `expect_download` subscribers, which outlive their aborted MCP task. No test drives `browser_tab_close` with real registrations.
- [ ] **One lock serialises every tool call.** It's held across CDP calls: a status check waited 3.8 s behind a pending element search, and a dialog can freeze the session. *(MCP #7, R)*
  - Test `mcp`: a status call during a pending find returns quickly.
- [ ] **`browser_screenshot` with a selector crops the wrong area on a scrolled page,** and `full_page` overrides `selector`. *(MCP #5, #9)* Test `mcp`.
- [ ] **`browser_type` reports success on elements you can't type into.** *(R)* Test `mcp`.
- [ ] **Tool list is about 215 KB (roughly 50k tokens)** because the selector description repeats across about 15 tools. Share one definition and shorten it. *(R)*
  - Test: a size budget assertion next to the schema snapshots.
- [ ] **Page HTML has no size limit** (305 KB on GitHub, 711–734 KB on YouTube). Add a default cap with a truncation marker plus a `max_bytes` input. *(R)* Test `mcp`. This changes I/O, so regenerate the schema snapshots.
- [ ] **"Suggested next step" hints** aren't where the README says, and many errors have none. *(R)* Test `unit` on the error mapping, plus a docs fix.
- [ ] **Smaller MCP bugs:** *(MCP #11–#14)*
  - a per-launch `stealth_profile` becomes the session default
  - `html_trim` matches tag prefixes and deletes custom elements
  - `content_type` doesn't replace a differently-cased header
  - stale tool descriptions

---

## P2: low severity

Each item needs a test; `unit` is fine where no browser is involved.

- [ ] **Browser:** *(BR #7–#12)*
  - `poll_initial_targets` can pick a non-page target as the main tab
  - the geo overlay overrides an explicit persona timezone
  - `BrowserContext` `Drop` panics outside a tokio runtime (`unit`)
  - `connect()` ignores tracker blocking and `proxy_auth`
  - `reconnect()` leaks interception actors
  - a target that detaches during the registrar stays registered forever
- [ ] **Browser:** a normal `close()` logs an ERROR "ws read failed". *(R)* Test `chrome`: no ERROR in the captured logs.
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
- [ ] **Query:** *(QE #11–#14)*
  - `g`/`y` regex flags skip matches
  - element-scoped `.xpath("//…")` searches the whole document
  - `<fieldset disabled>` not treated as disabled
  - polling gives up on transient navigation errors
- [ ] **Monitor/expect/cookies:** *(MEC #9–#11)*
  - subscriber tasks outlive their expectations
  - `save_to` has a fixed 30 s total limit
  - `Cookie::expires` is `Some(-1.0)` for session cookies
- [ ] **Transport/interception:** *(TI #12–#14)*
  - ~~lossy broadcast loses `requestPaused` events~~ Done 2026-11-06 (`8e99995a6`): the multiplexer reads the accounted bus, and after lost events disables and re-enables `Fetch`.
  - detach can run before attach finishes
  - ~~`UrlPattern` ignores backslash escapes~~ Done 2026-11-06 (`662bb63c4`): it matches as Chrome's `base::MatchPattern` (`?` is zero or one character, `\` escapes), and an XHR / Fetch / EventSource pattern covers all three.
- [ ] **Fetcher/WAF:** *(FW #6–#10)*
  - zip symlink directory creation (`unit` with a crafted zip)
  - partial downloads left behind
  - `Done` progress event fires early
  - Imperva `AlreadyClear { reese84 }` unreachable
  - `inject_captcha_solution` escaping
- [ ] **Stealth:** *(ST #18–#23)*
  - `codecs.js` says `'probably'` too often
  - WebGPU architecture wrong for 5 NVIDIA workstation cards
  - u64 seed loses precision in JS
  - `nearest_to_renderer` sibling match
  - `UaSpec` fields never applied
  - `deviceMemory` floor

---

## Docs

- [ ] **The README's first example and `examples/hello.rs` fail** because example.com was redesigned. *(R)* Point them at a stable target and add an examples smoke job (0.9).
- [x] **The mdBook interception chapter** claimed one actor per tab and got the default pattern's stage wrong. Rewritten for the ordered list in `0b7d285c2`..`777c35159`. *(TI out-of-scope note)*
- [x] **Remove the `(?i)` advice** from the rustdoc, the MCP descriptions and the book. Replaced with `(?i:…)` in `9e398a1dc`. *(QE #8, MCP #8)*
- [ ] **Any new public API** (the MCP HTML cap) needs the CLAUDE.md checklist: MCP tool or ledger entry, README tool count, rustdoc, mdBook.
