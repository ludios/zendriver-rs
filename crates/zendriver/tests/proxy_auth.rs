//! Proxy credentials (`BrowserBuilder::proxy_auth`), with the fixture server
//! as a proxy that wants Basic credentials.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use zendriver::Browser;
use zendriver_testkit::{
    FixtureServer, PROXY_PASSWORD, PROXY_USER, SITE_A, eval, launch_for_test, load,
};

/// The proxy gets the credentials; a site that asks for its own with a 401
/// doesn't.
#[tokio::test]
async fn proxy_credentials_go_to_the_proxy_and_not_to_a_site() {
    let server = FixtureServer::start().await;
    let builder = Browser::builder()
        .proxy(server.proxy_url())
        .proxy_auth(PROXY_USER, PROXY_PASSWORD);
    let browser = launch_for_test(builder).await;
    let tab = browser.main_tab();

    load(&tab, &server.url(SITE_A, "/plain")).await;
    assert_eq!(eval(&tab, "document.title").await, "Plain");
    let plain = server.requests_to("/plain");
    assert!(
        plain
            .iter()
            .any(|r| r.header("proxy-authorization").is_some()),
        "the proxy never got the credentials: {plain:?}"
    );

    // Headless Chrome fails a navigation to a 401 it has no credentials
    // for, with or without interception.
    let url = server.url(SITE_A, "/basic-auth");
    let res = tokio::time::timeout(Duration::from_secs(10), tab.goto(&url)).await;
    let site = server.requests_to("/basic-auth");
    assert!(!site.is_empty(), "/basic-auth was never requested");
    for request in &site {
        assert_eq!(
            request.header("authorization"),
            None,
            "the site got credentials: {request:?}"
        );
    }
    let err = res.expect("goto /basic-auth never returned").unwrap_err();
    assert!(
        err.to_string().contains("ERR_INVALID_AUTH_CREDENTIALS"),
        "{err}"
    );
    browser.close().await;
}

/// Chrome asks again each time the proxy rejects the credentials; answering
/// every time would send requests forever.
#[tokio::test]
async fn rejected_proxy_credentials_are_tried_once() {
    let server = FixtureServer::start().await;
    let builder = Browser::builder()
        .proxy(server.proxy_url())
        .proxy_auth(PROXY_USER, "wrong");
    let browser = launch_for_test(builder).await;
    let tab = browser.main_tab();

    let _ = tab.goto(server.url(SITE_A, "/plain")).await;
    tokio::time::timeout(Duration::from_secs(10), tab.wait_for_load())
        .await
        .expect("the navigation never finished")
        .expect("wait_for_load");
    tokio::time::sleep(Duration::from_secs(1)).await;
    // One without credentials, one with the wrong ones, then no more.
    let tries = server.requests_to("/plain");
    let sent: Vec<_> = tries
        .iter()
        .map(|r| r.header("proxy-authorization"))
        .collect();
    let wrong = format!("Basic {}", BASE64.encode(format!("{PROXY_USER}:wrong")));
    assert_eq!(sent, [None, Some(wrong.as_str())], "{tries:?}");
    assert_ne!(eval(&tab, "document.title").await, "Plain");
    browser.close().await;
}

/// A tab opened after launch, in the default context, gets the credentials
/// too.
#[tokio::test]
async fn a_later_tab_gets_the_proxy_credentials() {
    let server = FixtureServer::start().await;
    let builder = Browser::builder()
        .proxy(server.proxy_url())
        .proxy_auth(PROXY_USER, PROXY_PASSWORD);
    let browser = launch_for_test(builder).await;
    let tab = browser
        .new_tab()
        .await
        .unwrap_or_else(|e| panic!("new_tab: {e}"));

    load(&tab, &server.url(SITE_A, "/plain?title=Later")).await;
    assert_eq!(eval(&tab, "document.title").await, "Later");
    browser.close().await;
}
