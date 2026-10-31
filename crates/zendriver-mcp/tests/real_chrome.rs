//! `zendriver-mcp` driving a real Chrome, over stdio and over HTTP: a
//! browsing round trip works, and no way of stopping the server leaves its
//! Chrome running.
//!
//! Tests named `known_bug_*` reproduce an open bug, named in their
//! `#[ignore]` reason; the fix deletes the `#[ignore]` and the prefix.
//!
//! The server launches Chrome itself, without the `*.test` host mapping, so
//! these load the fixture pages from `127.0.0.1`.
//!
//! Linux only: [`ChromeTree`] finds the server's Chrome by command line,
//! and the shutdown tests send Unix signals.

#![cfg(all(feature = "integration-tests", target_os = "linux"))]
#![allow(clippy::panic)]

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use support::{McpServer, Transport};
use zendriver_testkit::{
    ChromeTree, FixtureServer, PROBE_JS, PROBE_PATH, assert_eventually, describe_disagreements,
    disagreements,
};

/// How long the server gets to exit once told to stop.
const SERVER_EXIT_WITHIN: Duration = Duration::from_secs(10);

/// How long Chrome's processes get to exit once the server has stopped or
/// closed the browser.
const CHROME_EXIT_WITHIN: Duration = Duration::from_secs(10);

/// Bound on waiting for a page to load.
const LOAD_WITHIN: Duration = Duration::from_secs(10);

/// Open a browser through `server` (with `open_args`) and load `url`,
/// waiting for its `load` event by polling the page.
async fn open_at(server: &McpServer, open_args: Value, url: &str) {
    server.call("browser_open", open_args).await;
    server.call("browser_goto", json!({ "url": url })).await;
    assert_eventually(LOAD_WITHIN, &format!("{url} finishes loading"), || async {
        let state = server
            .call(
                "browser_evaluate",
                json!({ "expression": "location.href + ' ' + document.readyState" }),
            )
            .await;
        state["value"] == format!("{url} complete")
    })
    .await;
}

/// Panic unless every process of `chrome` exits within
/// [`CHROME_EXIT_WITHIN`]. `after` says what should have made it exit. (The
/// survivors die with the [`McpServer`].)
async fn assert_chrome_exits(chrome: &ChromeTree, after: &str) {
    if let Err(survivors) = chrome.wait_gone(CHROME_EXIT_WITHIN).await {
        panic!(
            "{} Chrome process(es) still running {CHROME_EXIT_WITHIN:?} after {after}:\n  {}",
            survivors.len(),
            survivors.join("\n  "),
        );
    }
}

/// Load a page through `transport`, read it back, and close the browser.
async fn browse_and_close(transport: Transport) {
    let fixtures = FixtureServer::start().await;
    let server = McpServer::start(transport).await;
    let url = fixtures.url("127.0.0.1", "/plain?title=Hello");
    open_at(&server, json!({}), &url).await;
    let title = server
        .call(
            "browser_evaluate",
            json!({ "expression": "document.title" }),
        )
        .await;
    assert_eq!(title["value"], "Hello");

    let mut chrome = server.chrome();
    assert!(chrome.snapshot_by_command_line().await > 1);
    server.call("browser_close", json!({})).await;
    assert_chrome_exits(&chrome, "browser_close").await;
}

#[tokio::test]
async fn browse_and_close_over_stdio() {
    browse_and_close(Transport::Stdio).await;
}

#[tokio::test]
async fn browse_and_close_over_http() {
    browse_and_close(Transport::Http).await;
}

/// How a test stops the server.
#[derive(Clone, Copy, Debug)]
enum Stop {
    /// Disconnect the client: for stdio, close the server's stdin.
    Disconnect,
    /// Send a signal, e.g. `"TERM"`.
    Signal(&'static str),
}

/// With a page open over `transport`, stop the server by `stop`, and check
/// that the server exits and then its Chrome does too.
async fn chrome_exits_when_server_stops(transport: Transport, stop: Stop) {
    let fixtures = FixtureServer::start().await;
    let mut server = McpServer::start(transport).await;
    open_at(&server, json!({}), &fixtures.url("127.0.0.1", "/plain")).await;
    let mut chrome = server.chrome();
    assert!(chrome.snapshot_by_command_line().await > 1);

    match stop {
        Stop::Disconnect => server.close_stdin().await,
        Stop::Signal(signal) => server.signal(signal),
    }
    let what = format!("{stop:?} over {transport:?}");
    if server.wait_exit(SERVER_EXIT_WITHIN).await.is_none() {
        panic!(
            "server still running {SERVER_EXIT_WITHIN:?} after {what}; its log:\n{}",
            server.stderr()
        );
    }
    assert_chrome_exits(&chrome, &what).await;
}

#[tokio::test]
async fn chrome_exits_when_stdin_closes() {
    chrome_exits_when_server_stops(Transport::Stdio, Stop::Disconnect).await;
}

/// The server closes the browser, then hangs until its stdin closes.
#[tokio::test]
#[ignore = "known bug, TODO P0 'MCP shutdown leaves Chrome running': on SIGTERM the stdio \
            server closes Chrome but doesn't exit"]
async fn known_bug_chrome_exits_on_sigterm_over_stdio() {
    chrome_exits_when_server_stops(Transport::Stdio, Stop::Signal("TERM")).await;
}

#[tokio::test]
#[ignore = "known bug, TODO P0 'MCP shutdown leaves Chrome running': Chrome outlives a \
            SIGKILLed server"]
async fn known_bug_chrome_exits_on_sigkill_over_stdio() {
    chrome_exits_when_server_stops(Transport::Stdio, Stop::Signal("KILL")).await;
}

#[tokio::test]
#[ignore = "known bug, TODO P0 'MCP shutdown leaves Chrome running': the HTTP server \
            handles no signals, so Chrome outlives it"]
async fn known_bug_chrome_exits_on_sigterm_over_http() {
    chrome_exits_when_server_stops(Transport::Http, Stop::Signal("TERM")).await;
}

#[tokio::test]
#[ignore = "known bug, TODO P0 'MCP shutdown leaves Chrome running': Chrome outlives a \
            SIGKILLed server"]
async fn known_bug_chrome_exits_on_sigkill_over_http() {
    chrome_exits_when_server_stops(Transport::Http, Stop::Signal("KILL")).await;
}

/// `(field, why)`: a field of the stealth probe that disagrees under
/// `spoof_windows` because of an open bug, named by its TODO item. The same
/// as `spoofed win32` in `zendriver`'s `stealth_probe.rs`, which is what
/// `spoof_windows` launches.
const SPOOF_WINDOWS_KNOWN_BUGS: &[(&str, &str)] = &[
    (
        "deviceMemory",
        "P1 Stealth: worker GPU name and deviceMemory differ from the page under spoofed (R6)",
    ),
    (
        "webgl",
        "P1 Stealth: worker GPU name and deviceMemory differ from the page under spoofed (R6)",
    ),
    (
        "os",
        "P1 Stealth: JS-visible and HTTP identity come from different sources (ST #1, R6)",
    ),
    (
        "uaPlatform",
        "P1 Stealth: JS-visible and HTTP identity come from different sources (ST #1, R6)",
    ),
    (
        "platformVersion",
        "P1 Stealth: JS-visible and HTTP identity come from different sources (ST #1, R6)",
    ),
];

/// The stealth consistency probe (see `zendriver_testkit::disagreements`)
/// through MCP's `spoof_windows`: the disagreements are exactly the known
/// ones.
#[tokio::test]
async fn probe_agrees_under_spoof_windows() {
    let fixtures = FixtureServer::start().await;
    let server = McpServer::start(Transport::Stdio).await;
    let url = fixtures.url("127.0.0.1", PROBE_PATH);
    open_at(&server, json!({ "stealth_profile": "spoof_windows" }), &url).await;
    let probe = server
        .call("browser_evaluate_main", json!({ "expression": PROBE_JS }))
        .await;
    let found = disagreements(&probe["value"], &fixtures.requests());

    let chrome = server.chrome();
    server.call("browser_close", json!({})).await;
    assert_chrome_exits(&chrome, "browser_close").await;

    let found_fields: Vec<&str> = found.keys().copied().collect();
    let mut known: Vec<&str> = SPOOF_WINDOWS_KNOWN_BUGS.iter().map(|(f, _)| *f).collect();
    known.sort_unstable();
    assert_eq!(
        found_fields,
        known,
        "the disagreements aren't the known ones (SPOOF_WINDOWS_KNOWN_BUGS):\n{}",
        describe_disagreements(&found),
    );
}
