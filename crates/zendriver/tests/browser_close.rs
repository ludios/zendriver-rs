//! `Browser::close` against real Chrome.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use futures::StreamExt;
use tokio::time::timeout;
use tracing::Level;
use zendriver::Browser;
use zendriver_testkit::{FixtureServer, Logs, SITE_A, launch_for_test, load};

/// Far less than an expectation's default timeout.
const PROMPTLY: Duration = Duration::from_secs(5);

/// Chrome resets its DevTools sockets as it quits: the browser's, and each
/// tab's own. That is the expected end of a close, not a failure, so the
/// close must not log it as a warning or an error.
#[tokio::test]
async fn close_logs_no_warning_or_error() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    load(&browser.main_tab(), &server.url(SITE_A, "/plain")).await;
    let second = browser.new_tab().await.unwrap();
    load(&second, &server.url(SITE_A, "/plain")).await;

    let logs = Logs::from_now_on();
    browser.close().await;
    logs.assert_none_at_least(Level::WARN);
}

/// When Chrome quits, each tab's socket dies, and that ends what was
/// waiting on the tab: an expectation fails instead of waiting out its
/// timeout, and a monitor's stream ends.
#[tokio::test]
async fn close_ends_the_waits_on_a_tab() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    let expectation = tab.expect_request("never-requested");
    let mut monitor = tab.monitor().start().await.unwrap();

    browser.close().await;
    let matched = timeout(PROMPTLY, expectation.matched())
        .await
        .expect("the expectation outlived close()");
    assert!(matched.is_err(), "{matched:?}");
    timeout(PROMPTLY, async { while monitor.next().await.is_some() {} })
        .await
        .expect("the monitor's stream outlived close()");
}
