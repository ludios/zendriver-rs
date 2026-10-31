//! Waiting for a navigation, against real Chrome: each test navigates the
//! way a user does, waits the way the docs say to, and then checks that the
//! page it waited for has loaded. Ported from the `races.rs` and `kill.rs`
//! repros, with local fixtures instead of live sites.
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

use zendriver::{Browser, Key, SpecialKey, Tab};
use zendriver_testkit::{
    FixtureServer, SITE_A, TestBrowser, assert_not_before, launch_for_test, load,
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
    let answered = server.requests_to(SLOW_RESOURCE)[since..]
        .iter()
        .rev()
        .find_map(|r| r.responded_at)
        .unwrap_or_else(|| panic!("{wait} returned before the page's slow resource was answered"));
    assert_not_before(returned, answered, &format!("{wait}: the slow resource"));
    assert_loaded_now(tab, url, wait).await;
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

#[tokio::test]
#[ignore = "known bug, TODO P0 'wait_for_load() listens to the wrong events' (R1): after \
            Enter it returns at once, on the old page"]
async fn known_bug_wait_for_load_after_enter_waits_for_the_result_page() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/form?action=/slow-load")).await;
    let input = tab.find().css("#q").one().await.expect("find #q");
    input.type_text("a b").await.expect("type");
    input
        .press(Key::Special(SpecialKey::Enter))
        .await
        .expect("press Enter");
    tab.wait_for_load().await.expect("wait_for_load");
    let url = server.url(SITE_A, "/slow-load?q=a+b");
    assert_waited_for(
        &server,
        &tab,
        &url,
        0,
        Instant::now(),
        "Enter + wait_for_load",
    )
    .await;
    browser.close().await;
}

#[tokio::test]
#[ignore = "known bug, TODO P0 'wait_for_load() listens to the wrong events' (R1): after a \
            click on a link it returns at once, on the old page"]
async fn known_bug_wait_for_load_after_a_click_waits_for_the_linked_page() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain?link=/slow-load")).await;
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
