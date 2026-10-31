//! Waiting for a navigation, against real Chrome: each test navigates the
//! way a user does, waits the way the docs say to, and then checks that the
//! page it waited for has loaded. Ported from the `races.rs` repro, with
//! local fixtures instead of live sites.
//!
//! Not here: `wait_for_load` after Enter or a link click, with no stale
//! signal from an earlier `goto`. On live sites it returned on the old page,
//! but locally the navigation starts before `wait_for_load` looks, so it
//! passes except on a loaded machine. That race needs the planned
//! `expect_navigation`, which subscribes before the action.
//!
//! The page waited for is usually a `/slow-load`, whose `load` event comes a
//! second late, after the server answers its image. A wait that returns
//! early is caught by comparing when it returned with when the image was
//! answered, not by racing the navigation.
//!
//! Tests named `known_bug_*` reproduce an open bug, named in their
//! `#[ignore]` reason; the fix deletes the `#[ignore]` and the prefix.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use std::time::Instant;

use zendriver::{Browser, Tab};
use zendriver_testkit::{
    FixtureServer, SITE_A, TestBrowser, assert_not_before, launch_for_test, load, wait_loaded,
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
#[ignore = "known bug, TODO P0 'wait_for_load() listens to the wrong events' (TAB #1): it \
            returns when the iframe finishes loading"]
async fn known_bug_wait_for_load_waits_for_the_main_frame_not_an_iframe() {
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
#[ignore = "known bug, TODO P0 'new_tab_at(url) + wait_for_load() returns while the tab is \
            still blank' (R1)"]
async fn known_bug_new_tab_at_then_wait_for_load_waits_for_the_page() {
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

/// A `goto` nobody waited for, then a navigation that a fresh wait would
/// handle: the document arrives at once, so only the stale signal is wrong.
#[tokio::test]
#[ignore = "known bug, TODO P0 'wait_for_load() listens to the wrong events' (TAB #2): it \
            takes the signal of an earlier goto that nobody waited for"]
async fn known_bug_wait_for_load_ignores_an_earlier_gotos_signal() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    let first = server.url(SITE_A, "/plain?link=/slow-load");
    tab.goto(&first).await.expect("goto");
    wait_loaded(&tab, &first).await;
    let link = tab.find().css("#link").one().await.expect("find #link");
    link.click().await.expect("click");
    tab.wait_for_load().await.expect("wait_for_load");
    let url = server.url(SITE_A, "/slow-load");
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
/// with it, Chrome restores the page so fast that the bug doesn't show.
/// `fresh=1`, or the slow image would come from the cache.
#[tokio::test]
#[ignore = "known bug, TODO P0 'back() / forward() / reload() don't wait' (R1)"]
async fn known_bug_back_and_forward_wait_for_the_page() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder().arg("--disable-back-forward-cache")).await;
    let tab = browser.main_tab();
    let first = server.url(SITE_A, "/slow-load?fresh=1&title=First");
    let second = server.url(SITE_A, "/slow-load?fresh=1&title=Second");
    load(&tab, &first).await;
    load(&tab, &second).await;

    let since = slow_requests(&server);
    tab.back().await.expect("back");
    assert_waited_for(&server, &tab, &first, since, Instant::now(), "back").await;
    let since = slow_requests(&server);
    tab.forward().await.expect("forward");
    assert_waited_for(&server, &tab, &second, since, Instant::now(), "forward").await;
    browser.close().await;
}

#[tokio::test]
#[ignore = "known bug, TODO P0 'back() / forward() / reload() don't wait' (R1)"]
async fn known_bug_reload_waits_for_the_page() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    let url = server.url(SITE_A, "/slow-load?fresh=1");
    load(&tab, &url).await;
    let since = slow_requests(&server);
    tab.reload().await.expect("reload");
    assert_waited_for(&server, &tab, &url, since, Instant::now(), "reload").await;
    browser.close().await;
}
