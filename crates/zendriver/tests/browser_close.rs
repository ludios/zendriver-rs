//! `Browser::close` against real Chrome.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use tracing::Level;
use zendriver::Browser;
use zendriver_testkit::{FixtureServer, SITE_A, launch_for_test, load};

/// Chrome resets every socket it has open as it quits, so `close()` sees
/// each of them drop. That is the expected end of a close, not a failure, so
/// it must not be logged as a warning or an error.
#[tokio::test]
async fn close_logs_no_warning_or_error() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    load(&browser.main_tab(), &server.url(SITE_A, "/plain")).await;
    let second = browser.new_tab().await.unwrap();
    load(&second, &server.url(SITE_A, "/plain")).await;

    let logs = browser.logs().clone();
    browser.close().await;
    logs.assert_none_at_least(Level::WARN);
}
