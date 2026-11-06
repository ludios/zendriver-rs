//! Waiting for a navigation, against real Chrome: each test navigates the
//! way a user does, waits the way the docs say to, and then checks that the
//! page it waited for has loaded. Ported from the `races.rs` repro, with
//! local fixtures instead of live sites.
//!
//! A navigation that Enter or a link click starts is waited for with
//! `expect_navigation`, armed before the action, or `wait_for_url`, which
//! may be called after it. (`wait_for_load` after such an action isn't
//! tested: on live sites it returned on the old page, but locally the
//! navigation starts before `wait_for_load` looks, so it would pass except
//! on a loaded machine.)
//!
//! The page waited for is usually a `/slow-load`, whose `load` event comes a
//! second late, after the server answers its image. A wait that returns
//! early is caught by comparing when it returned with when the image was
//! answered, not by racing the navigation.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use std::time::{Duration, Instant};

use serde_json::json;

use zendriver::{Browser, Key, SpecialKey, Tab, ZendriverError};
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
/// surely started (the race that `expect_navigation` is for).
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

/// `reload` of a tab whose renderer crashed doesn't wait for the renderer:
/// it needs only the browser process, which handles `Page.reload`. (Whether
/// Chrome then recovers the tab varies, and the tab's session answers no
/// renderer command after the crash, even after a reload; see TODO.md.)
#[tokio::test]
async fn reload_of_a_crashed_tab_needs_no_renderer() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    // The renderer crashes before it can answer, and answers nothing after.
    let crash = tab.session().call("Page.crash", json!({}));
    let _ = tokio::time::timeout(Duration::from_secs(2), crash).await;
    let answer = tokio::time::timeout(Duration::from_secs(1), tab.evaluate_main::<i64>("1")).await;
    assert!(
        answer.is_err(),
        "the renderer answered after the crash: {answer:?}"
    );
    tokio::time::timeout(Duration::from_secs(10), tab.reload())
        .await
        .expect("reload of a crashed tab waited for the renderer")
        .expect("reload");
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

/// A form submitted with Enter: nothing in zendriver follows the navigation
/// the key starts, so `expect_navigation` is armed before the key press.
#[tokio::test]
async fn expect_navigation_waits_for_the_page_a_form_submits_to() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/form")).await;
    let input = tab.find().css("#q").one().await.expect("the form's input");
    input.type_text("hello").await.expect("type_text");
    let navigation = tab.expect_navigation();
    input
        .press(Key::Special(SpecialKey::Enter))
        .await
        .expect("press Enter");
    let reached = navigation.await.expect("expect_navigation");
    let url = server.url(SITE_A, "/form-result?q=hello");
    assert_eq!(reached.url.as_str(), url);
    assert!(!reached.same_document);
    assert_loaded_now(&tab, &url, "expect_navigation").await;
    browser.close().await;
}

/// A link to a slow page: the wait ends only when that page has loaded, not
/// when its document has arrived.
#[tokio::test]
async fn expect_navigation_waits_for_the_page_a_link_leads_to() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain?link=/slow-load")).await;
    tab.wait_for_idle().await.expect("wait_for_idle");
    let url = server.url(SITE_A, "/slow-load");
    let since = slow_requests(&server);
    let navigation = tab.expect_navigation();
    let link = tab.find().css("#link").one().await.expect("the link");
    link.click().await.expect("click");
    let reached = navigation.await.expect("expect_navigation");
    assert_waited_for(
        &server,
        &tab,
        &url,
        since,
        Instant::now(),
        "expect_navigation",
    )
    .await;
    assert_eq!(reached.url.as_str(), url);
    assert!(!reached.same_document);
    browser.close().await;
}

/// The History API and a fragment change navigate within the document: the
/// wait ends at once, and says so.
#[tokio::test]
async fn expect_navigation_ends_on_a_same_document_navigation() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    let cases = [
        (
            "history.pushState(null, '', '/plain?title=Pushed'); 1",
            "/plain?title=Pushed",
        ),
        ("location.hash = 'h'; 1", "/plain?title=Pushed#h"),
    ];
    for (js, path) in cases {
        let navigation = tab.expect_navigation();
        eval(&tab, js).await;
        let reached = navigation.await.unwrap_or_else(|e| panic!("{js}: {e}"));
        assert_eq!(reached.url.as_str(), server.url(SITE_A, path), "{js}");
        assert!(reached.same_document, "{js}");
    }
    browser.close().await;
}

/// `wait_for_url` after the click that navigates: being at a URL is a state,
/// so calling it after the action misses nothing. (The link is encoded so
/// that the pattern can't match the page the click starts from.)
#[tokio::test]
async fn wait_for_url_after_a_click_waits_for_the_page_at_the_url() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain?link=%2Fslow-load")).await;
    let url = server.url(SITE_A, "/slow-load");
    let since = slow_requests(&server);
    let link = tab.find().css("#link").one().await.expect("the link");
    link.click().await.expect("click");
    let reached = tab.wait_for_url("/slow-load").await.expect("wait_for_url");
    assert_waited_for(&server, &tab, &url, since, Instant::now(), "wait_for_url").await;
    // Whether the new document had already committed when the wait began
    // depends on timing, so `same_document` may be either.
    assert_eq!(reached.url.as_str(), url);
    browser.close().await;
}

/// A tab already at a matching URL: the wait ends at once, on that document.
#[tokio::test]
async fn wait_for_url_ends_at_once_on_a_page_already_at_the_url() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    let url = server.url(SITE_A, "/plain");
    load(&tab, &url).await;
    let started = Instant::now();
    let reached = tab.wait_for_url("/plain").await.expect("wait_for_url");
    let took = started.elapsed();
    assert!(took < Duration::from_secs(2), "wait_for_url took {took:?}");
    assert_eq!(reached.url.as_str(), url);
    assert!(reached.same_document);
    browser.close().await;
}

/// The pages on the way to the URL don't end the wait: here a `/hop` page
/// that commits, loads, and only then leaves for the destination. (The
/// destination is encoded within the hop's URL, so that the pattern can't
/// match the hop.)
#[tokio::test]
async fn wait_for_url_skips_the_pages_before_the_url() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    let start = server.url(
        SITE_A,
        "/plain?link=/hop%3Fto%3D%252Fplain%253Ftitle%253DFinal",
    );
    load(&tab, &start).await;
    let link = tab.find().css("#link").one().await.expect("the link");
    link.click().await.expect("click");
    let reached = tab.wait_for_url("title=Final").await.expect("wait_for_url");
    let url = server.url(SITE_A, "/plain?title=Final");
    assert_eq!(reached.url.as_str(), url);
    assert_eq!(
        server.requests_to("/hop").len(),
        1,
        "the hop page was visited"
    );
    assert_loaded_now(&tab, &url, "wait_for_url").await;
    browser.close().await;
}

/// A URL that never comes: the wait ends at its timeout.
#[tokio::test]
async fn wait_for_url_ends_at_its_timeout() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    let res = tab
        .wait_for_url("/nowhere")
        .timeout(Duration::from_millis(300))
        .await;
    assert!(matches!(res, Err(ZendriverError::Timeout(_))), "{res:?}");
    browser.close().await;
}

/// A link to a port nothing listens on: the navigation ends on Chrome's
/// error page, which fails the wait instead of ending it.
#[tokio::test]
async fn expect_navigation_fails_on_a_page_that_cant_be_reached() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    let start = server.url(SITE_A, "/plain?link=http%3A%2F%2F127.0.0.1%3A9%2Fnope");
    load(&tab, &start).await;
    let navigation = tab.expect_navigation();
    let link = tab.find().css("#link").one().await.expect("the link");
    link.click().await.expect("click");
    let res = navigation.await;
    assert!(
        matches!(&res, Err(ZendriverError::Navigation(m)) if m.contains("127.0.0.1:9/nope")),
        "{res:?}"
    );
    browser.close().await;
}

/// A new tab whose first navigation is still pending when the wait begins:
/// the tab is at `about:blank`, whatever Chrome reports as its URL, so the
/// wait ends only when the page has loaded.
#[tokio::test]
async fn wait_for_url_after_new_tab_at_waits_for_the_page() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let url = server.url(SITE_A, "/slow-load?first_byte_ms=500");
    let tab = browser.new_tab_at(&url).await.expect("new_tab_at");
    let reached = tab.wait_for_url("/slow-load").await.expect("wait_for_url");
    assert_waited_for(&server, &tab, &url, 0, Instant::now(), "wait_for_url").await;
    assert_eq!(reached.url.as_str(), url);
    browser.close().await;
}

/// A document that commits at another URL and changes it to a matching one
/// while still loading: the wait ends when that document has loaded.
#[tokio::test]
async fn wait_for_url_waits_for_the_load_of_a_document_whose_url_changes() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    let waiting = tab.wait_for_url("pushed=1");
    let since = slow_requests(&server);
    tab.goto(&server.url(SITE_A, "/slow-load"))
        .await
        .expect("goto");
    // Once the new document is there, change its URL before its load ends.
    tab.find().css("#heading").one().await.expect("the heading");
    eval(
        &tab,
        "history.pushState(null, '', '/slow-load?pushed=1'); 1",
    )
    .await;
    let reached = waiting.await.expect("wait_for_url");
    let url = server.url(SITE_A, "/slow-load?pushed=1");
    assert_waited_for(&server, &tab, &url, since, Instant::now(), "wait_for_url").await;
    assert_eq!(reached.url.as_str(), url);
    assert!(!reached.same_document);
    browser.close().await;
}
