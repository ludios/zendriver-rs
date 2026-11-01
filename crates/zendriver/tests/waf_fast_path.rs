//! The WAF bypasses' `with_interception()` fast path against real Chrome and
//! the fixture server.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use serde_json::json;
use zendriver::{Browser, ImpervaClearanceOutcome};
use zendriver_testkit::axum::Router;
use zendriver_testkit::axum::routing::get;
use zendriver_testkit::{FixtureServer, SITE_A, eval, launch_for_test, load};

/// Every sensor request completes while the Imperva bypass watches for
/// one, including those after the first 2xx (the fast path once left them
/// paused).
#[tokio::test]
async fn imperva_fast_path_leaves_sensor_requests_alone() {
    let sensor = Router::new().route("/_Incapsula_Resource", get(|| async { "sensor" }));
    let server = FixtureServer::start_with(sensor).await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;

    // A legacy Incapsula cookie and no reese84 token: the bypass polls
    // until its timeout, while the page asks its sensor twice.
    eval(
        &tab,
        "document.cookie = 'incap_ses_1=x';
         window.sensor = [];
         for (const [query, delay] of [['first', 300], ['second', 800]]) {
           setTimeout(() => fetch('/_Incapsula_Resource?' + query)
             .then(r => sensor.push(query + ' ' + r.status)), delay);
         }",
    )
    .await;
    let outcome = tab
        .imperva()
        .with_interception()
        .timeout(Duration::from_secs(2))
        .wait_for_clearance()
        .await
        .unwrap();
    assert!(
        matches!(outcome, ImpervaClearanceOutcome::TimedOut { .. }),
        "{outcome:?}"
    );

    // Both fetches were sent 1.2 s or more before the timeout.
    assert_eq!(
        eval(&tab, "sensor").await,
        json!(["first 200", "second 200"])
    );
    browser.close().await;
}
