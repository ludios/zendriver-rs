//! `Browser::close` and `Tab::close` against real Chrome.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use futures::StreamExt;
use tokio::time::timeout;
use tracing::Level;
use zendriver::{Browser, NetworkDeliveryBoundary, NetworkEvent, ZendriverError};
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

/// When Chrome quits, the socket of a tab that has its own socket dies, and
/// that ends what was waiting on the tab: an expectation fails with
/// `EventStreamIncomplete` instead of waiting out its timeout, and a
/// monitor's last event is the disconnect.
#[tokio::test]
async fn close_ends_the_waits_on_a_tab() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    let expectation = tab.expect_request("never-requested");
    let monitor = tab.monitor().start().await.unwrap();

    browser.close().await;
    let matched = timeout(PROMPTLY, expectation.matched())
        .await
        .expect("the expectation outlived close()");
    assert!(
        matches!(matched, Err(ZendriverError::EventStreamIncomplete)),
        "{matched:?}"
    );
    let events: Vec<NetworkEvent> = timeout(PROMPTLY, monitor.collect())
        .await
        .expect("the monitor's stream outlived close()");
    assert!(
        matches!(
            events.last(),
            Some(NetworkEvent::DeliveryBoundary(
                NetworkDeliveryBoundary::Disconnected { .. }
            ))
        ),
        "{events:?}"
    );
}

/// `Tab::close` shuts the socket of a tab that has its own socket down
/// before Chrome closes the tab, and that ends what was waiting on the tab:
/// an expectation fails with `EventStreamIncomplete` instead of waiting out
/// its timeout, and a monitor's last event is the shutdown.
#[tokio::test]
async fn tab_close_ends_the_waits_on_the_tab() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.new_tab().await.unwrap();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    let expectation = tab.expect_request("never-requested");
    let monitor = tab.monitor().start().await.unwrap();

    tab.close().await.unwrap();
    let matched = timeout(PROMPTLY, expectation.matched())
        .await
        .expect("the expectation outlived the tab");
    assert!(
        matches!(matched, Err(ZendriverError::EventStreamIncomplete)),
        "{matched:?}"
    );
    let events: Vec<NetworkEvent> = timeout(PROMPTLY, monitor.collect())
        .await
        .expect("the monitor's stream outlived the tab");
    assert!(
        matches!(
            events.last(),
            Some(NetworkEvent::DeliveryBoundary(
                NetworkDeliveryBoundary::Shutdown { .. }
            ))
        ),
        "{events:?}"
    );
    browser.close().await;
}
