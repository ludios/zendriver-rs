# zendriver-rs

## About this fork

This is [ludios/zendriver-rs](https://github.com/ludios/zendriver-rs), an LLM-authored fork of [TurtIeSocks/zendriver-rs](https://github.com/TurtIeSocks/zendriver-rs). It began with an AI review of upstream's code and with runs against real sites, and it fixes the bugs that those found, most of them with a test that runs against real Chrome.

The fork branched from upstream in September 2026 and has none of upstream's later changes. The main one that it lacks: upstream now attaches out-of-process iframes and gives them the stealth setup.

The fork isn't published. The crates on crates.io, the docs.rs pages and the book that the rest of this README links to are all upstream's, and the fork's API differs from upstream's in places (interception most of all). To use the fork:

```bash
cargo add zendriver --git https://github.com/ludios/zendriver-rs
cargo install zendriver-mcp --git https://github.com/ludios/zendriver-rs
```

What the fork does better than upstream:

- **Navigation waits wait for the right page.** Upstream's `wait_for_load` ends when any frame stops loading, so an iframe ends it, and so does a navigation that finished earlier. Here it follows the main frame's own navigation, also after `back` / `forward` / `reload` and in a new tab. New: `Tab::expect_navigation` and `Tab::wait_for_url`.
- **No skipped CDP events.** Upstream's subscribers each filter one lossy broadcast of the whole connection, in which a slow subscriber loses events. Here a subscriber gets its session's events in the order Chrome sent them, and a stream that can't continue ends with its reason (overflow, detach, shutdown) instead of skipping events. The load waits, `wait_for_idle`, `expect_*`, the network monitor and interception all read these streams.
- **Proxy credentials go only to the proxy.** Upstream answers every HTTP auth challenge with them, so a site that answers 401 receives the proxy's username and password.
- **Interception rules that work together.** Upstream runs one `Fetch` actor per rule set: the actors replace each other's patterns, race to answer each paused request, and turn `Fetch` off for all of them when one is dropped. Here each tab has one ordered list of rules and streams (tracker blocking included), the first entry that fits a paused request decides it, and Chrome pauses only the requests that some entry acts on. A `modify_response` rule runs without the caller also naming the Response stage; upstream's needs that, so the `modify_response` action of upstream's MCP server does nothing.
- **Stealth with far less JavaScript in the page.** Nine of upstream's injected patches are deleted (`webdriver`, `plugins`, `permissions`, `chrome`, `webrtc`, `screen`, `codecs`, `broken_image`, `mouse`): given the right launch flags and CDP overrides, Chrome reports those values itself, with no replaced function for a detector to find. (`mouse.js` was itself a tell: it sent pages mouse events with `isTrusted === false`.) `native()` now injects no script at all, and `spoofed()` patches only what Chrome can't be told: WebGL / WebGPU, `deviceMemory`, and readback noise.
- **Headless Chrome looks like a desktop.** Launch switches give it a screen, window and viewport that agree, instead of JS getters on `screen`. On Linux it also gets a mouse, so pages no longer match `(pointer: none)` and `(hover: none)`.
- **Finding and using elements stays out of the page's own JavaScript.** Queries and element actions run in the isolated world. On a test page that records calls to its wrapped DOM methods, finding, clicking and typing made 66 recorded calls before this change, and makes none now. Scrolling uses Chrome's `DOM.scrollIntoViewIfNeeded`, which doesn't re-centre an element that is already in view.
- **Element queries and checks that agree with Chrome.**
  - `role()` asks Chrome's accessibility tree, so it finds implicit roles and elements in shadow roots. Upstream's finds only `role="…"` attributes: 0 of 32 ordinary links on a test page.
  - The hit test before a click sees into shadow roots, and sees an overlay that covers an iframe.
  - `visible_only` keeps elements below the fold, which upstream's (and so its MCP `browser_click`) can't find.
  - An invalid selector or regex fails at once with `JsException`. Upstream polls until the timeout and reports `ElementNotFound`.
  - `Frame::evaluate_main` runs in the iframe's own main world. Upstream's runs in the top page for an iframe in the page's process.
- **Calls that always failed now work.** `Browser::connect("http://…")`, and the browser-level cookie jar's `for_url` and `delete`. A tab in an isolated `BrowserContext` now reads that context's cookies, not the default context's.
- **Downloads outlive their tab.** A download is followed on the browser's connection, so closing its tab doesn't lose it, and closing a tab no longer resets where the other tabs' downloads go.
- **MCP server.** `browser_back` / `_forward` / `_reload` wait for the page; `browser_wait_for_url` and `wait_for_navigation` (on `browser_click` / `browser_press`) are new; agents see and position rules in the ordered interception list; and over HTTP, SIGTERM closes every session's Chrome instead of leaving them running.
- **`BrowserBuilder::env` / `env_remove`** set and remove environment variables for the launched Chrome.
- **Tests against real Chrome.** `zendriver-testkit` gives every such test a local fixture server (two sites, an authenticating proxy) and checks on close that Chrome's whole process tree exited. A contract test sends every CDP command that the library uses to a real Chrome, to prove that the command exists at the scope where the library sends it. A stealth probe compares the identity that the page, an iframe, a worker and the HTTP headers each report.

The rest of this README is upstream's, updated where the fork changed what it describes.

---

Async-first browser automation via the Chrome DevTools Protocol, with a coherent stealth identity and explicit anti-detection controls on by default — drive real Chrome from Rust, or hand the keys to an LLM agent over the [Model Context Protocol](https://modelcontextprotocol.io/).

[![crates.io](https://img.shields.io/crates/v/zendriver.svg)](https://crates.io/crates/zendriver)
[![docs.rs](https://docs.rs/zendriver/badge.svg)](https://docs.rs/zendriver)
[![Book](https://img.shields.io/badge/book-mdBook-blue)](https://turtiesocks.github.io/zendriver-rs/)
[![MCP](https://img.shields.io/badge/MCP-server-blue)](https://turtiesocks.github.io/zendriver-rs/mcp.html)
[![MSRV 1.85](https://img.shields.io/badge/rustc-1.85+-lightgray.svg)](https://blog.rust-lang.org/2025/02/20/Rust-1.85.0/)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](#license)
[![CI](https://github.com/TurtIeSocks/zendriver-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/TurtIeSocks/zendriver-rs/actions/workflows/ci.yml)

A Rust port of [zendriver](https://github.com/cdpdriver/zendriver). Drives Chrome via raw CDP — no WebDriver, no JS shim — with anti-detection patches baked in by default.

📖 **[User guide & full documentation →](https://turtiesocks.github.io/zendriver-rs/)** · 🦀 **[API reference (docs.rs) →](https://docs.rs/zendriver)** · 🤖 **[MCP server for AI agents →](https://turtiesocks.github.io/zendriver-rs/mcp.html)**

## Quick example

```rust,no_run
use zendriver::Browser;

#[tokio::main]
async fn main() -> zendriver::Result<()> {
    let browser = Browser::builder().headless(true).launch().await?;
    let tab = browser.main_tab();

    tab.goto("https://example.com").await?;
    tab.wait_for_load().await?;

    // Find by visible text (auto-waits up to the selector's timeout).
    let link = tab.find().text("More information...").one().await?;
    // Arm the wait before the click: it ends once the linked page has loaded.
    let navigation = tab.expect_navigation();
    link.click().await?;
    navigation.await?;

    // Read back from the page's main world.
    let title: String = tab.evaluate_main("document.title").await?;
    println!("title: {title}");

    browser.close().await?;
    Ok(())
}
```

More working examples in [`crates/zendriver/examples/`](crates/zendriver/examples/).

## Feature matrix

| Feature        | Default? | Use case                                                      | Extra deps                            |
| -------------- | -------- | ------------------------------------------------------------- | ------------------------------------- |
| `stealth`      | yes      | Anti-detection: spoofed UA/platform, a mouse (not "no pointer") for headless Chrome on Linux, full-surface WebGL parameter spoof resolved from measured GPU capability tiers, a 482-device catalogue to name or draw a specific GPU, isolated worlds, JS shim, opt-in `gpu_backend` for real-GPU WebGL/WebGPU rendering | (built-in to `zendriver`)             |
| `interception` | no       | Block/modify requests via CDP `Fetch.*`; rule-based + streams | `zendriver-interception`              |
| `expect`       | no       | Playwright-style `expect_response()` / `expect_request()`     | (in-tree, no extra crate)             |
| `cloudflare`   | no       | Solve Cloudflare Turnstile challenges                         | `zendriver-cloudflare`                |
| `imperva`      | no       | Imperva WAF / Incapsula bypass (reese84 / legacy / CAPTCHA)   | `zendriver-imperva`                   |
| `datadome`     | no       | DataDome bypass (device-check / CAPTCHA / block) + `Surface::Webgpu` coherence | `zendriver-datadome` |
| `monitor`      | no       | Passive network monitor — buffered HTTP / WebSocket / SSE events via `tab.monitor()`, plus opt-in incremental body streaming | (in-tree, no extra crate)             |
| `geo`          | no       | Country code → coherent `locale` + `languages` (Accept-Language) + representative `timezone` persona overlay, plus opt-in `geo_auto()` exit-IP auto-resolution (exact probe timezone, not just representative) | `reqwest` (via `zendriver-stealth`) |
| `tracker-blocking` | no   | Opt-in third-party tracker / fingerprinter host blocklist (curated bundled list + BYO file / URL) | `reqwest` + `dirs`         |
| `fetcher`      | no       | Auto-download a pinned Chromium build — Chrome for Testing (default), ungoogled-chromium, or a Chromium snapshot | `zendriver-fetcher` + `reqwest`/`zip` |

Separate binary crate (not a feature on `zendriver`):

| Crate           | Use case                                                                                                                         | Install                                                                                     |
| --------------- | -------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| `zendriver-mcp` | Drive a stealth Chrome from any LLM agent — [Model Context Protocol](https://modelcontextprotocol.io/) server, 74 tools, stdio + streamable HTTP | `cargo install zendriver-mcp` ([docs](https://turtiesocks.github.io/zendriver-rs/mcp.html)) |

## Install

Pick the use case that matches what you're building.

**Just browse:**

```bash
cargo add zendriver
```

Default stealth is on.

**Stealth scraping (explicit):**

```bash
cargo add zendriver --features stealth
```

Same as above — only spell out the feature if you want it visible in `Cargo.toml`.

**Everything:**

```bash
cargo add zendriver --features "interception expect cloudflare imperva datadome monitor geo tracker-blocking fetcher"
```

Adds request interception, `expect()` matchers, Cloudflare Turnstile bypass, Imperva WAF / Incapsula bypass, DataDome bypass, the passive network monitor, country→locale geo overlay (explicit `geo_locale()` or auto-resolved via `geo_auto()`), the opt-in tracker/fingerprinter blocklist, and the Chrome for Testing fetcher.

## Drive a stealth browser from your AI agent

`zendriver-mcp` is a **first-class [Model Context Protocol](https://modelcontextprotocol.io/) server** that hands the entire zendriver-rs surface to any LLM client — Claude Desktop, Claude Code, Cursor, or your own agent loop. **74 tools**, two transports (stdio + streamable HTTP), and the same stealth-by-default fingerprinting baked into the lib. Unlike generic browser MCP servers, this one bypasses Cloudflare Turnstile, ships an isolated-world JS eval that survives anti-bot detection, and lets agents persist auth state across sessions.

```bash
cargo install zendriver-mcp
```

**Claude Desktop / Claude Code config:**

```json
{
  "mcpServers": {
    "zendriver": {
      "command": "zendriver-mcp"
    }
  }
}
```

**Even easier — the [Claude Code plugin](plugins/zendriver/):**

```bash
claude plugin marketplace add TurtIeSocks/zendriver-rs
claude plugin install zendriver@zendriver-rs
# then, in a session:
/zendriver:setup     # prebuilt (no Rust), source, or link
```

No manual MCP config — the plugin bundles the server plus scraping skills, the `/zendriver:scrape` and `/zendriver:extract` commands, and a `zendriver-scraper` subagent.

**What agents get out of the box:**

- **Stealth navigation** — `browser_open`, `browser_goto`, `browser_back/forward/reload`, `browser_wait_for_idle`, `browser_wait_for_url` (waits out a login that bounces through another site), `wait_for_navigation` on `browser_click` / `browser_press`, plus a runtime-swappable `browser_set_stealth_profile` (auto / native / spoof_macos / spoof_linux / spoof_windows), an `input_profile` (native / coherent) opt-in for humanized keyboard/mouse timing decoupled from the stealth profile, and a `gpu_backend` (disabled / swift_shader / native) opt-in to render WebGL/WebGPU on the host's real GPU instead of the software fallback
- **Selector-based find + actions** — one `Selector` arg works across `browser_find`, `browser_click`, `browser_type`, `browser_press`, `browser_set_value`, `browser_upload`, etc., with CSS / XPath / visible-text / ARIA-role lookups and per-frame scoping
- **Three ways to "see" the page** — `browser_html` (trimmed DOM), `browser_screenshot` (PNG / JPEG / WebP as inline image content), `browser_element_state` (visibility / geometry / attrs)
- **Stateful primitives** agents need for real work — `browser_cookies_persist` for save/load auth, full `browser_storage_*`, multi-tab management, frame traversal; `browser_monitor_*` passive network monitor (HTTP / WS / SSE); `browser_request` for HTTP from the page's own session
- **Anti-bot superpowers** (gated cargo features, on by default for the published binary):
  - `browser_solve_turnstile` — Cloudflare Turnstile bypass without a CAPTCHA-solving service, with the widget's `selectors` and `click_policy` as request fields so an agent can follow Cloudflare's markup changes without waiting on a release
  - `browser_open` `block_trackers` / `tracker_blocklist` — opt-in third-party tracker & fingerprinter host blocking (curated bundled list + bring-your-own file / URL)
  - `browser_solve_datadome` — DataDome device-check / CAPTCHA / block bypass (with `Surface::Webgpu` coherence)
  - `browser_intercept_*` — continue/block/redirect/respond/modify requests via CDP `Fetch.*`, as one ordered list per tab that the agent reads and places each rule in (ahead of or behind the tracker blocking)
  - `browser_expect_register / _await` — Playwright-style "wait for response/dialog/download" matchers, split across MCP calls so the agent can act in between
  - `browser_install_chrome` — pull a pinned Chrome-for-Testing build on demand
- **Actionable errors** — an error that a tool can help with carries an `_meta.suggested_next` hint pointing the agent at that tool (e.g. `ElementNotFound` → "try `browser_html` to inspect")

See the [MCP chapter](https://turtiesocks.github.io/zendriver-rs/mcp.html) for the full tool reference, CLI flags, HTTP-mode operator notes, and troubleshooting guide.

## Phases

Six development phases shipped into the v0.1.0 release. The [mdBook](https://turtiesocks.github.io/zendriver-rs/) covers each surface in depth.

1. **Foundation** — CDP transport + minimal `Browser`/`Tab`/`Element`. See [introduction](https://turtiesocks.github.io/zendriver-rs/introduction.html).
2. **Stealth** — fingerprint patches + isolated worlds + stealth JS bundle. See [stealth](https://turtiesocks.github.io/zendriver-rs/stealth.html).
3. **Element API completeness** — CSS/XPath/text/role selectors, actionability, input controller, screenshots. See [quickstart](https://turtiesocks.github.io/zendriver-rs/quickstart.html).
4. **`Tab`/`Browser` completeness** — multi-tab, cookies, storage, frames, nav history, `wait_for_idle`, per-context isolation with first-class per-context proxy + auth (`browser.browser_context().proxy(...).proxy_auth(...).build()`). See [multi-tab](https://turtiesocks.github.io/zendriver-rs/multi-tab.html) + [frames](https://turtiesocks.github.io/zendriver-rs/frames.html) + [browser-context](https://turtiesocks.github.io/zendriver-rs/browser-context.html).
5. **Optional gated features** — request interception, `expect()` matchers, Cloudflare / Imperva / DataDome bypass, network monitor, geo locale overlay, tracker blocklist, Chrome-for-Testing fetcher. See [interception](https://turtiesocks.github.io/zendriver-rs/interception.html), [expect](https://turtiesocks.github.io/zendriver-rs/expect.html), [cloudflare](https://turtiesocks.github.io/zendriver-rs/cloudflare.html), [fetcher](https://turtiesocks.github.io/zendriver-rs/fetcher.html).
6. **Polish + release** — trait extraction, rustdoc + mdBook, publish to crates.io.

## Comparison

| Feature                  | zendriver-rs            | chromiumoxide     | fantoccini      | headless_chrome | thirtyfour      |
| ------------------------ | ----------------------- | ----------------- | --------------- | --------------- | --------------- |
| API ergonomics _opinion_ | builder + auto-wait     | raw CDP types     | WebDriver verbs | sync wrappers   | WebDriver verbs |
| Stealth out-of-box       | yes (default)           | no                | no              | no              | no              |
| Multi-tab                | yes (first-class)       | yes               | yes             | yes             | yes             |
| Interception             | yes (`Fetch.*` wrapper) | yes (raw)         | no (proxy-only) | partial         | no (proxy-only) |
| Cloudflare bypass        | yes (`zendriver-cloudflare`) | no           | no              | no              | no              |
| MCP server for AI agents | yes (`zendriver-mcp`, 74 tools) | no        | no              | no              | no              |
| License                  | MIT OR Apache-2.0       | MIT OR Apache-2.0 | Apache-2.0      | MIT             | MIT             |
| Async runtime            | tokio                   | tokio / async-std | tokio           | sync            | tokio           |

Subjective rows marked `*opinion`. All claims accurate as of the 0.1.0 release; check upstream changelogs before relying on them.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Issues and PRs welcome.

## License

Dual-licensed under MIT ([LICENSE-MIT](LICENSE-MIT)) and Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)) at your option.
