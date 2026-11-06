//! `Browser::reconnect` against real Chrome and the fixture server.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use zendriver::Browser;
use zendriver_testkit::{FixtureServer, SITE_A, assert_eventually, eval, launch_for_test, load};

/// The tabs from before a reconnect are closed: a call on one fails, and a
/// rule set on one no longer answers the target's requests, which the
/// rebuilt tab now sees.
#[tokio::test]
async fn reconnect_closes_the_tabs_from_before() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let old = browser.main_tab();
    load(&old, &server.url(SITE_A, "/plain")).await;
    let _rule = old
        .intercept()
        .respond("*/plain?old-rule", 200, Vec::new(), b"old rule".to_vec())
        .unwrap()
        .start()
        .await
        .unwrap();

    browser.reconnect().await.unwrap();
    assert_eventually(Duration::from_secs(5), "the tab is rebuilt", || async {
        !browser.tabs().await.is_empty()
    })
    .await;
    let tab = browser.tabs().await.remove(0);
    let body = eval(&tab, "fetch('/plain?old-rule').then(r => r.text())").await;
    assert!(
        body.as_str().unwrap().contains("<title>Plain</title>"),
        "{body}"
    );
    assert!(old.evaluate_main::<i64>("1").await.is_err());
    browser.close().await;
}
