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

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{McpServer, Transport};
use zendriver_testkit::{
    ChromeTree, FixtureServer, PROBE_JS, PROBE_PATH, assert_eventually, assert_not_before,
    check_known_disagreements, disagreements,
};

/// How long the server gets to exit once told to stop.
const SERVER_EXIT_WITHIN: Duration = Duration::from_secs(10);

/// How long Chrome's processes get to exit once the server has stopped or
/// closed the browser.
const CHROME_EXIT_WITHIN: Duration = Duration::from_secs(10);

/// Bound on waiting for a page to load.
const LOAD_WITHIN: Duration = Duration::from_secs(10);

/// Open a browser through `server` (with `open_args`) and load `url`,
/// waiting for its `load` event by polling the page. A poll that fails, as
/// one can while the page swaps documents, is tried again.
async fn open_at(server: &mut McpServer, open_args: Value, url: &str) {
    server.open(open_args).await;
    server.call("browser_goto", json!({ "url": url })).await;
    assert_eventually(LOAD_WITHIN, &format!("{url} finishes loading"), || async {
        let state = server
            .try_call(
                "browser_evaluate",
                json!({ "expression": "location.href + ' ' + document.readyState" }),
            )
            .await;
        state.is_ok_and(|s| s["value"] == format!("{url} complete"))
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
    let mut server = McpServer::start(transport).await;
    let url = fixtures.url("127.0.0.1", "/plain?title=Hello");
    open_at(&mut server, json!({}), &url).await;
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
    open_at(&mut server, json!({}), &fixtures.url("127.0.0.1", "/plain")).await;
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

/// The server closes the browser and exits, though its stdin stays open.
#[tokio::test]
async fn chrome_exits_on_sigterm_over_stdio() {
    chrome_exits_when_server_stops(Transport::Stdio, Stop::Signal("TERM")).await;
}

#[tokio::test]
#[ignore = "known bug, TODO P0 'MCP shutdown leaves Chrome running': Chrome outlives a \
            SIGKILLed server"]
async fn known_bug_chrome_exits_on_sigkill_over_stdio() {
    chrome_exits_when_server_stops(Transport::Stdio, Stop::Signal("KILL")).await;
}

#[tokio::test]
async fn chrome_exits_on_sigterm_over_http() {
    chrome_exits_when_server_stops(Transport::Http, Stop::Signal("TERM")).await;
}

#[tokio::test]
#[ignore = "known bug, TODO P0 'MCP shutdown leaves Chrome running': Chrome outlives a \
            SIGKILLed server"]
async fn known_bug_chrome_exits_on_sigkill_over_http() {
    chrome_exits_when_server_stops(Transport::Http, Stop::Signal("KILL")).await;
}

/// The stealth consistency probe (see `zendriver_testkit::disagreements`)
/// through MCP's `spoof_windows`, which launches what `zendriver`'s
/// `stealth_probe.rs` calls `spoofed win32`: the disagreements are that
/// profile's known ones.
///
/// Leaves out the hardware fields, which disagree here only on some hosts:
/// `browser_open` takes a whole persona, not an overlay, so this can't pin
/// them as `stealth_probe.rs` does.
#[tokio::test]
async fn probe_agrees_under_spoof_windows() {
    let fixtures = FixtureServer::start().await;
    let mut server = McpServer::start(Transport::Stdio).await;
    let url = fixtures.url("127.0.0.1", PROBE_PATH);
    open_at(
        &mut server,
        json!({ "stealth_profile": "spoof_windows" }),
        &url,
    )
    .await;
    let probe = server
        .call("browser_evaluate_main", json!({ "expression": PROBE_JS }))
        .await;
    let found = disagreements(&probe["value"], &fixtures.requests());

    let chrome = server.chrome();
    server.call("browser_close", json!({})).await;
    assert_chrome_exits(&chrome, "browser_close").await;

    let host_dependent = ["hardwareConcurrency", "deviceMemory"];
    if let Err(mismatch) = check_known_disagreements("spoofed win32", &found, &host_dependent) {
        panic!("{mismatch}");
    }
}

/// `browser_back` and `browser_reload` report the page that they navigated
/// to, once its `load` event has fired (MCP #4).
#[tokio::test]
async fn history_tools_wait_for_the_page_they_reach() {
    let fixtures = FixtureServer::start().await;
    let mut server = McpServer::start(Transport::Stdio).await;
    let first = fixtures.url("127.0.0.1", "/plain?title=First");
    open_at(&mut server, json!({}), &first).await;
    let second = fixtures.url("127.0.0.1", "/plain?title=Second");
    server.call("browser_goto", json!({ "url": second })).await;
    let back = server.call("browser_back", json!({})).await;
    assert_eq!(
        (back["url"].as_str(), back["title"].as_str()),
        (Some(first.as_str()), Some("First")),
        "browser_back reported {back}"
    );

    // `fresh=1`: each load of the page asks for the slow resource anew.
    let slow = fixtures.url("127.0.0.1", "/slow-load?fresh=1");
    server.call("browser_goto", json!({ "url": slow })).await;
    let since = fixtures.requests_to("/slow-resource").len();
    server.call("browser_reload", json!({})).await;
    let returned = Instant::now();
    let answered = fixtures
        .last_responded_at_after("/slow-resource", since)
        .expect("browser_reload returned before the page's slow resource was answered");
    assert_not_before(returned, answered, "browser_reload: the slow resource");
}

/// How long a selector tool may look for an element before giving up.
const FIND_WITHIN_MS: u64 = 2000;

/// `.below` also matches a `display: none` button earlier in the page,
/// which the default `visible_only` skips.
#[tokio::test]
async fn click_reaches_a_button_below_the_fold() {
    let fixtures = FixtureServer::start().await;
    let mut server = McpServer::start(Transport::Stdio).await;
    open_at(
        &mut server,
        json!({}),
        &fixtures.url("127.0.0.1", "/below-fold"),
    )
    .await;
    server
        .call(
            "browser_click",
            json!({ "css": ".below", "timeout_ms": FIND_WITHIN_MS }),
        )
        .await;
    let clicked = server
        .call(
            "browser_evaluate_main",
            json!({ "expression": "window.belowClicked === true" }),
        )
        .await;
    assert_eq!(clicked["value"], true, "the click didn't reach #below");
}

#[tokio::test]
async fn scroll_into_view_finds_an_off_screen_element() {
    let fixtures = FixtureServer::start().await;
    let mut server = McpServer::start(Transport::Stdio).await;
    open_at(
        &mut server,
        json!({}),
        &fixtures.url("127.0.0.1", "/below-fold"),
    )
    .await;
    server
        .call(
            "browser_scroll_into_view",
            json!({ "css": "#below", "timeout_ms": FIND_WITHIN_MS }),
        )
        .await;
    let visible = server
        .call(
            "browser_evaluate",
            json!({ "expression":
                "document.getElementById('below').getBoundingClientRect().top < innerHeight" }),
        )
        .await;
    assert_eq!(visible["value"], true, "#below is still off screen");
}

/// The names of the cookies in a `browser_cookies_get` result.
fn cookie_names(result: &Value) -> Vec<String> {
    result["cookies"]
        .as_array()
        .expect("a cookies array")
        .iter()
        .map(|c| c["name"].as_str().expect("a cookie name").to_string())
        .collect()
}

#[tokio::test]
async fn cookies_get_by_url_and_delete() {
    let fixtures = FixtureServer::start().await;
    let mut server = McpServer::start(Transport::Stdio).await;
    let url = fixtures.url("127.0.0.1", "/plain");
    open_at(&mut server, json!({}), &url).await;
    server
        .call(
            "browser_evaluate_main",
            json!({ "expression": "document.cookie = 'flavour=oat'" }),
        )
        .await;
    let all = server.call("browser_cookies_get", json!({})).await;
    assert_eq!(
        cookie_names(&all),
        ["flavour"],
        "browser_cookies_get without a url"
    );

    let by_url = server
        .call("browser_cookies_get", json!({ "url": url }))
        .await;
    assert_eq!(
        cookie_names(&by_url),
        ["flavour"],
        "browser_cookies_get by url"
    );
    server
        .call(
            "browser_cookies_delete",
            json!({ "name": "flavour", "domain": "127.0.0.1" }),
        )
        .await;
    let left = server.call("browser_cookies_get", json!({})).await;
    assert!(
        cookie_names(&left).is_empty(),
        "browser_cookies_delete left {left}"
    );
}

/// `browser_click` with `wait_for_navigation` replies once the page the
/// click leads to has loaded, so the reply describes that page.
#[tokio::test]
async fn click_with_wait_for_navigation_reports_the_page_it_leads_to() {
    let fixtures = FixtureServer::start().await;
    let mut server = McpServer::start(Transport::Stdio).await;
    // `fresh=1`: each load of the page asks for the slow resource anew.
    let slow = fixtures.url("127.0.0.1", "/slow-load?fresh=1");
    let start = fixtures.url("127.0.0.1", "/plain?link=/slow-load%3Ffresh%3D1");
    open_at(&mut server, json!({}), &start).await;
    let since = fixtures.requests_to("/slow-resource").len();
    let reply = server
        .call(
            "browser_click",
            json!({
                "css": "#link",
                "wait_for_navigation": true,
                "return_snapshot": true,
                "timeout_ms": FIND_WITHIN_MS,
            }),
        )
        .await;
    let returned = Instant::now();
    let answered = fixtures
        .last_responded_at_after("/slow-resource", since)
        .expect("browser_click returned before the page's slow resource was answered");
    assert_not_before(returned, answered, "browser_click: the slow resource");
    let snapshot = reply["snapshot"].as_str().expect("a snapshot");
    assert!(
        snapshot.contains("Slow load"),
        "snapshot of the old page: {snapshot}"
    );
    let state = server
        .call(
            "browser_evaluate",
            json!({ "expression": "location.href + ' ' + document.readyState" }),
        )
        .await;
    assert_eq!(state["value"], format!("{slow} complete"));
}

/// `browser_press` with `wait_for_navigation`: Enter submits the form, and
/// the reply describes the result page.
#[tokio::test]
async fn press_with_wait_for_navigation_reports_the_page_it_leads_to() {
    let fixtures = FixtureServer::start().await;
    let mut server = McpServer::start(Transport::Stdio).await;
    open_at(&mut server, json!({}), &fixtures.url("127.0.0.1", "/form")).await;
    server
        .call(
            "browser_type",
            json!({ "css": "#q", "text": "hello", "timeout_ms": FIND_WITHIN_MS }),
        )
        .await;
    let reply = server
        .call(
            "browser_press",
            json!({
                "css": "#q",
                "key": "Enter",
                "wait_for_navigation": true,
                "return_snapshot": true,
                "timeout_ms": FIND_WITHIN_MS,
            }),
        )
        .await;
    let snapshot = reply["snapshot"].as_str().expect("a snapshot");
    assert!(
        snapshot.contains("Form result"),
        "snapshot of the old page: {snapshot}"
    );
    let state = server
        .call(
            "browser_evaluate",
            json!({ "expression": "location.href + ' ' + document.readyState" }),
        )
        .await;
    let url = fixtures.url("127.0.0.1", "/form-result?q=hello");
    assert_eq!(state["value"], format!("{url} complete"));
}

/// `browser_wait_for_url` after a click: it skips the `/hop` page on the way
/// and reports the page at the URL.
#[tokio::test]
async fn wait_for_url_reports_the_page_at_the_url() {
    let fixtures = FixtureServer::start().await;
    let mut server = McpServer::start(Transport::Stdio).await;
    // The destination is encoded within the hop's URL, so that the pattern
    // can't match the hop.
    let start = fixtures.url(
        "127.0.0.1",
        "/plain?link=/hop%3Fto%3D%252Fplain%253Ftitle%253DFinal",
    );
    open_at(&mut server, json!({}), &start).await;
    server
        .call(
            "browser_click",
            json!({ "css": "#link", "timeout_ms": FIND_WITHIN_MS }),
        )
        .await;
    let reached = server
        .call(
            "browser_wait_for_url",
            json!({ "url_substr": "title=Final" }),
        )
        .await;
    let url = fixtures.url("127.0.0.1", "/plain?title=Final");
    assert_eq!(
        (reached["url"].as_str(), reached["title"].as_str()),
        (Some(url.as_str()), Some("Final")),
        "browser_wait_for_url reported {reached}"
    );
    assert_eq!(
        fixtures.requests_to("/hop").len(),
        1,
        "the hop page was visited"
    );
    let err = server
        .try_call("browser_wait_for_url", json!({}))
        .await
        .expect_err("neither url_substr nor url_regex");
    assert!(err.contains("exactly one"), "{err}");
}
