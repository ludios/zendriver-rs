//! Waiting for a navigation, against real Chrome: each test navigates the
//! way a user does, waits the way the docs say to, and then checks that the
//! page it waited for has loaded. Ported from the `races.rs` repro, with
//! local fixtures instead of live sites.
//!
//! Not here: `wait_for_load` after Enter or a link click, with no earlier
//! `goto`. On live sites it returned on the old page, but locally the
//! navigation starts before `wait_for_load` looks, so it passes except on a
//! loaded machine. That race needs the planned `expect_navigation`, which
//! subscribes before the action.
//!
//! The page waited for is usually a `/slow-load`, whose `load` event comes a
//! second late, after the server answers its image. A wait that returns
//! early is caught by comparing when it returned with when the image was
//! answered, not by racing the navigation.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use std::time::{Duration, Instant};

use serde_json::json;

use zendriver::{Browser, Tab};
use zendriver_testkit::{
    FixtureServer, SITE_A, TestBrowser, assert_eventually, assert_not_before, eval,
    launch_for_test, load, wait_loaded,
};

/// The path whose answer sets when `/slow-load`'s `load` event can fire.
const SLOW_RESOURCE: &str = "/slow-resource";

async fn launch() -> TestBrowser {
    launch_for_test(Browser::builder()).await
}

/// How many requests for the slow resource `server` has seen so far: pass
/// it as `since` to [`assert_waited_for`].
fn slow_requests(server: &FixtureServer) -> usize {
    server.requests_to(SLOW_RESOURCE).len()
}

/// Panic unless `tab` is at `url` with its `load` event fired, checked once,
/// right after `wait` returned.
async fn assert_loaded_now(tab: &Tab, url: &str, wait: &str) {
    let state = tab
        .evaluate_main::<String>("location.href + ' ' + document.readyState")
        .await
        .unwrap_or_else(|e| format!("evaluate failed: {e}"));
    assert_eq!(state, format!("{url} complete"), "right after {wait}");
}

/// Panic unless a wait that returned at `returned` waited for `server` to
/// answer a request for the slow resource made after the first `since`, and
/// `tab` is now at `url` with its `load` event fired. `wait` names the wait,
/// for the message.
async fn assert_waited_for(
    server: &FixtureServer,
    tab: &Tab,
    url: &str,
    since: usize,
    returned: Instant,
    wait: &str,
) {
    let answered = server
        .last_responded_at_after(SLOW_RESOURCE, since)
        .unwrap_or_else(|| panic!("{wait} returned before the page's slow resource was answered"));
    assert_not_before(returned, answered, &format!("{wait}: the slow resource"));
    assert_loaded_now(tab, url, wait).await;
}

#[tokio::test]
async fn goto_then_wait_for_load_waits_for_the_page() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    let url = server.url(SITE_A, "/slow-load");
    tab.goto(&url).await.expect("goto");
    tab.wait_for_load().await.expect("wait_for_load");
    assert_waited_for(
        &server,
        &tab,
        &url,
        0,
        Instant::now(),
        "goto + wait_for_load",
    )
    .await;
    browser.close().await;
}

#[tokio::test]
async fn wait_for_load_waits_for_the_main_frame_not_an_iframe() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    let url = server.url(SITE_A, "/slow-load?iframe=1");
    tab.goto(&url).await.expect("goto");
    tab.wait_for_load().await.expect("wait_for_load");
    assert_waited_for(
        &server,
        &tab,
        &url,
        0,
        Instant::now(),
        "goto + wait_for_load",
    )
    .await;
    browser.close().await;
}

#[tokio::test]
async fn new_tab_at_then_wait_for_load_waits_for_the_page() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    // Live sites take a while to send the document; until it arrives, the
    // new tab is a loaded about:blank.
    let url = server.url(SITE_A, "/slow-load?first_byte_ms=500");
    let tab = browser.new_tab_at(&url).await.expect("new_tab_at");
    tab.wait_for_load().await.expect("wait_for_load");
    assert_waited_for(
        &server,
        &tab,
        &url,
        0,
        Instant::now(),
        "new_tab_at + wait_for_load",
    )
    .await;
    browser.close().await;
}

/// Opening a tab at `about:blank`, however spelled, arms nothing: it has
/// loaded by the time the call returns.
#[tokio::test]
async fn new_blank_tab_then_wait_for_load_returns_at_once() {
    let browser = launch().await;
    for url in ["about:blank", "", "ABOUT:BLANK"] {
        let tab = browser.new_tab_at(url).await.expect("new_tab_at");
        let started = Instant::now();
        tab.wait_for_load().await.expect("wait_for_load");
        let took = started.elapsed();
        assert!(
            took < Duration::from_secs(5),
            "{url:?}: wait_for_load took {took:?}"
        );
    }
    browser.close().await;
}

/// `Frame::goto` arms the main frame's wait as `Tab::goto` does, before
/// navigating, so the wait neither returns early nor waits for a load that
/// already happened (FI #2).
#[tokio::test]
async fn main_frame_goto_then_wait_for_load_waits_for_the_page() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    let main = tab.main_frame().await.expect("main_frame");
    let url = server.url(SITE_A, "/slow-load");
    main.goto(&url).await.expect("Frame::goto");
    main.wait_for_load().await.expect("Frame::wait_for_load");
    assert_waited_for(
        &server,
        &tab,
        &url,
        0,
        Instant::now(),
        "Frame::goto + Frame::wait_for_load",
    )
    .await;

    let url = server.url(SITE_A, "/plain");
    main.goto(&url).await.expect("Frame::goto");
    wait_loaded(&tab, &url).await;
    let started = Instant::now();
    main.wait_for_load()
        .await
        .expect("Frame::wait_for_load after the load");
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "Frame::wait_for_load took {took:?}"
    );
    browser.close().await;
}

/// A `goto` nobody waited for, then a link click: the wait is for the
/// clicked page, not the first one. Waits until the server has the clicked
/// page's request before calling `wait_for_load`, so its navigation has
/// surely started (a race that the planned `expect_navigation` handles).
#[tokio::test]
async fn wait_for_load_ignores_an_earlier_gotos_signal() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    let first = server.url(SITE_A, "/plain?link=/slow-load%3Ffirst_byte_ms%3D500");
    tab.goto(&first).await.expect("goto");
    wait_loaded(&tab, &first).await;
    let link = tab.find().css("#link").one().await.expect("find #link");
    link.click().await.expect("click");
    assert_eventually(
        Duration::from_secs(5),
        "the clicked page is requested",
        || async { !server.requests_to("/slow-load").is_empty() },
    )
    .await;
    tab.wait_for_load().await.expect("wait_for_load");
    let url = server.url(SITE_A, "/slow-load?first_byte_ms=500");
    assert_waited_for(
        &server,
        &tab,
        &url,
        0,
        Instant::now(),
        "click + wait_for_load",
    )
    .await;
    browser.close().await;
}

/// Without the back-forward cache, as for the many pages it won't keep:
/// with it, Chrome restores the page so fast that an early return wouldn't
/// show. `fresh=1`, or the slow image would come from the cache.
#[tokio::test]
async fn back_and_forward_then_wait_for_load_wait_for_the_page() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder().arg("--disable-back-forward-cache")).await;
    let tab = browser.main_tab();
    let first = server.url(SITE_A, "/slow-load?fresh=1&title=First");
    let second = server.url(SITE_A, "/slow-load?fresh=1&title=Second");
    load(&tab, &first).await;
    load(&tab, &second).await;

    let since = slow_requests(&server);
    tab.back().await.expect("back");
    tab.wait_for_load().await.expect("wait_for_load");
    assert_waited_for(&server, &tab, &first, since, Instant::now(), "back").await;
    let since = slow_requests(&server);
    tab.forward().await.expect("forward");
    tab.wait_for_load().await.expect("wait_for_load");
    assert_waited_for(&server, &tab, &second, since, Instant::now(), "forward").await;
    browser.close().await;
}

#[tokio::test]
async fn reload_then_wait_for_load_waits_for_the_page() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    let url = server.url(SITE_A, "/slow-load?fresh=1");
    load(&tab, &url).await;
    let since = slow_requests(&server);
    tab.reload().await.expect("reload");
    tab.wait_for_load().await.expect("wait_for_load");
    assert_waited_for(&server, &tab, &url, since, Instant::now(), "reload").await;
    browser.close().await;
}

/// A tab whose renderer crashed can be reloaded: `reload` needs only the
/// browser process. Checked through the browser's connection, because the
/// tab's own session answers no renderer command after the crash, even once
/// the reload has loaded the page (see TODO.md).
#[tokio::test]
async fn reload_recovers_a_crashed_tab() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    let url = server.url(SITE_A, "/plain");
    load(&tab, &url).await;
    // The renderer crashes before it can answer.
    let crash = tab.session().call("Page.crash", json!({}));
    let _ = tokio::time::timeout(Duration::from_secs(2), crash).await;
    tokio::time::timeout(Duration::from_secs(10), tab.reload())
        .await
        .expect("reload of a crashed tab hung")
        .expect("reload");
    let target = json!({ "targetId": tab.target_id() });
    assert_eventually(
        Duration::from_secs(10),
        "the reload reaches the page",
        || {
            let target = target.clone();
            let browser = &browser;
            let url = &url;
            async move {
                let info = browser
                    .cdp()
                    .call_raw("Target.getTargetInfo", target, None)
                    .await;
                info.is_ok_and(|info| info["targetInfo"]["url"] == url.as_str())
            }
        },
    )
    .await;
    browser.close().await;
}

/// History navigations that load nothing over the network still end the
/// wait: back within one document (after `pushState`), and back to a page
/// that the back-forward cache kept (on by default).
#[tokio::test]
async fn back_within_a_document_or_from_the_bfcache_then_wait_for_load() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    let first = server.url(SITE_A, "/plain?title=First");
    load(&tab, &first).await;
    eval(&tab, "history.pushState({}, '', '/pushed'); 1").await;
    tab.back().await.expect("back");
    tab.wait_for_load().await.expect("wait_for_load");
    assert_loaded_now(&tab, &first, "back within the document").await;

    load(&tab, &server.url(SITE_A, "/plain?title=Second")).await;
    tab.back().await.expect("back");
    tab.wait_for_load().await.expect("wait_for_load");
    assert_loaded_now(&tab, &first, "back from the bfcache").await;
    browser.close().await;
}

/// A `goto` that Chrome refuses outright starts no navigation, so the next
/// `wait_for_load` has nothing to wait for.
#[tokio::test]
async fn wait_for_load_after_a_refused_goto_returns_at_once() {
    let browser = launch().await;
    let tab = browser.main_tab();
    tab.goto("not a url")
        .await
        .expect_err("goto an invalid URL");
    let started = Instant::now();
    tab.wait_for_load().await.expect("wait_for_load");
    let took = started.elapsed();
    assert!(took < Duration::from_secs(5), "wait_for_load took {took:?}");
    browser.close().await;
}
