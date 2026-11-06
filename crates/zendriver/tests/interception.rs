//! Network interception (`Tab::intercept`) against real Chrome and the
//! fixture server.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use zendriver::{Browser, ResponseOverrides};
use zendriver_testkit::{
    FixtureServer, PROXY_PASSWORD, PROXY_USER, SITE_A, eval, launch_for_test, load,
};

/// What a `fetch` of `path` from the page gets:
/// `"<status> [<statusText>] <content-type>"`, or `"timeout"` after 5 s (a
/// request left paused never settles).
async fn fetch_response(tab: &zendriver::Tab, path: &str) -> String {
    let js = format!(
        "Promise.race([\
           fetch('{path}').then(r =>\
             `${{r.status}} [${{r.statusText}}] ${{r.headers.get('content-type')}}`),\
           new Promise(done => setTimeout(() => done('timeout'), 5000)),\
         ])"
    );
    eval(tab, &js).await.as_str().unwrap().to_string()
}

/// A response rule that changes only the status keeps the upstream headers
/// (Chrome refuses a `continueResponse` with a status but no headers), and a
/// rule that asks for an empty phrase gets one (Chrome refuses `""`).
#[tokio::test]
async fn response_rules_that_set_only_a_status_or_a_phrase() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;

    let _intercept = tab
        .intercept()
        .pattern("*/plain?*")
        .at_response()
        .modify_response("*/plain?status", |_| ResponseOverrides {
            status: Some(203),
            ..ResponseOverrides::default()
        })
        .unwrap()
        .modify_response("*/plain?no-phrase", |_| ResponseOverrides {
            phrase: Some(String::new()),
            ..ResponseOverrides::default()
        })
        .unwrap()
        .start()
        .await
        .unwrap();

    assert_eq!(
        fetch_response(&tab, "/plain?status").await,
        "203 [Non-Authoritative Information] text/html; charset=utf-8"
    );
    assert_eq!(
        fetch_response(&tab, "/plain?no-phrase").await,
        "200 [] text/html; charset=utf-8"
    );
    browser.close().await;
}

/// `start` returns once Chrome intercepts: a request the page sends right
/// after it is already answered by the rule.
#[tokio::test]
async fn a_request_right_after_start_is_intercepted() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;

    let _mock = tab
        .intercept()
        .respond("*/plain?mocked", 200, Vec::new(), b"Mocked".to_vec())
        .unwrap()
        .start()
        .await
        .unwrap();
    let body = eval(&tab, "fetch('/plain?mocked').then(r => r.text())").await;
    assert_eq!(body, "Mocked");
    browser.close().await;
}

/// A rule of the caller's and the browser's proxy credentials work on the
/// same tab, and stopping the rule leaves the credentials working.
#[tokio::test]
async fn a_rule_and_the_proxy_credentials_compose() {
    let server = FixtureServer::start().await;
    let builder = Browser::builder()
        .proxy(server.proxy_url())
        .proxy_auth(PROXY_USER, PROXY_PASSWORD);
    let browser = launch_for_test(builder).await;
    let tab = browser.main_tab();

    let page = b"<title>Mocked</title>".to_vec();
    let html = vec![("content-type".to_string(), "text/html".to_string())];
    let mock = tab
        .intercept()
        .respond("*/plain?mocked", 200, html, page)
        .unwrap()
        .start()
        .await
        .unwrap();
    load(&tab, &server.url(SITE_A, "/plain?mocked")).await;
    assert_eq!(eval(&tab, "document.title").await, "Mocked");
    load(&tab, &server.url(SITE_A, "/plain")).await;
    assert_eq!(eval(&tab, "document.title").await, "Plain");

    mock.stop().await.unwrap();
    load(&tab, &server.url(SITE_A, "/plain?mocked")).await;
    assert_eq!(eval(&tab, "document.title").await, "Plain");
    browser.close().await;
}

/// A response rule needs no pattern of its own: the layer pauses at the
/// stage each rule acts at, so `modify_response` alone changes the
/// response, and a block rule after it still blocks.
#[tokio::test]
async fn rules_pause_at_their_own_stage() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;

    let _rules = tab
        .intercept()
        .modify_response("*/plain?*", |_| ResponseOverrides {
            status: Some(203),
            ..ResponseOverrides::default()
        })
        .unwrap()
        .block("*/plain?blocked")
        .unwrap()
        .start()
        .await
        .unwrap();
    assert_eq!(
        fetch_response(&tab, "/plain?modified").await,
        "203 [Non-Authoritative Information] text/html; charset=utf-8"
    );
    let blocked = "fetch('/plain?blocked').then(() => 'loaded', () => 'blocked')";
    assert_eq!(eval(&tab, blocked).await, "blocked");
    browser.close().await;
}
