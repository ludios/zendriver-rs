//! Network interception (`Tab::intercept`) against real Chrome and the
//! fixture server.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use zendriver::{Browser, ResponseOverrides};
use zendriver_testkit::{FixtureServer, SITE_A, eval, launch_for_test, load};

/// How long `Fetch.enable` takes to reach Chrome after `start()`, which
/// doesn't wait for it.
const INTERCEPT_SETTLE: Duration = Duration::from_millis(150);

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
        .start();
    tokio::time::sleep(INTERCEPT_SETTLE).await;

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
