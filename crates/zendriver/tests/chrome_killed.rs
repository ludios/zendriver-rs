//! A `Browser` whose Chrome was killed: every call fails promptly with
//! `Disconnected` instead of hanging, and `close()` returns. Ported from the
//! `kill.rs` repro.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use std::time::Duration;

use tokio::time::timeout;
use zendriver::{Browser, ZendriverError};
use zendriver_testkit::{ChromeTree, FixtureServer, load};

/// What "promptly" allows: far less than any of zendriver's own timeouts.
const PROMPTLY: Duration = Duration::from_secs(5);

/// Panic unless `result` is the error a call on a dead browser gives.
fn assert_disconnected<T: std::fmt::Debug>(call: &str, result: zendriver::Result<T>) {
    assert!(
        matches!(result, Err(ZendriverError::Disconnected)),
        "{call} after Chrome was killed: expected Disconnected, got {result:?}"
    );
}

/// Not `launch_for_test`: its `close()` checks that Chrome was still running.
#[tokio::test]
async fn calls_fail_promptly_once_chrome_is_killed() {
    let server = FixtureServer::start().await;
    let browser = Browser::builder()
        .headless(true)
        .launch()
        .await
        .expect("launch");
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/plain")).await;

    let mut chrome = ChromeTree::of(&browser).await;
    chrome.snapshot(&browser).await;
    chrome.kill();

    let url = server.url("127.0.0.1", "/plain?title=After");
    let goto = timeout(PROMPTLY, tab.goto(&url)).await.expect("goto hung");
    assert_disconnected("goto", goto);
    let evaluate = timeout(PROMPTLY, tab.evaluate_main::<String>("location.href"))
        .await
        .expect("evaluate_main hung");
    assert_disconnected("evaluate_main", evaluate);
    let find = timeout(PROMPTLY, tab.find().css("h1").one())
        .await
        .expect("find hung");
    assert_disconnected("find", find);
    timeout(PROMPTLY, browser.close())
        .await
        .expect("close hung")
        .expect("close");
    // Until `close()`, the browser process is a zombie that zendriver
    // hasn't reaped yet.
    chrome
        .wait_gone(PROMPTLY)
        .await
        .expect("close() reaps the killed Chrome");
}
