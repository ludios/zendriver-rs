//! Downloads against real Chrome: where they go, and how long zendriver
//! can follow them.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use std::path::Path;
use std::time::Duration;

use zendriver::{Browser, Tab};
use zendriver_testkit::{
    DOWNLOAD_BODY, FixtureServer, SITE_A, assert_eventually, launch_for_test, load,
};

/// Far more than a local download takes.
const PROMPTLY: Duration = Duration::from_secs(10);

/// Click `selector` on `tab`.
async fn click(tab: &Tab, selector: &str) {
    tab.find()
        .css(selector)
        .one()
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
}

/// Wait until `path` holds `body`.
async fn assert_file_holds(path: &Path, body: &[u8]) {
    let what = format!("{} holds the download", path.display());
    assert_eventually(PROMPTLY, &what, || async {
        tokio::fs::read(path).await.is_ok_and(|read| read == body)
    })
    .await;
}

/// A download path set from a tab that has its own socket stays after
/// that tab closes. Chrome keeps a setting sent on a tab's socket only as
/// long as that tab's session, so zendriver sends it on the browser's.
#[tokio::test]
async fn a_download_path_outlives_the_tab_that_set_it() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let dir = tempfile::tempdir().unwrap();
    let setter = browser.new_tab().await.unwrap();
    assert!(
        setter.session().is_root(),
        "the tab has no socket of its own"
    );
    setter.set_download_path(dir.path()).await.unwrap();
    setter.close().await.unwrap();

    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/downloads")).await;
    click(&tab, "#download").await;
    assert_file_holds(&dir.path().join("report.csv"), DOWNLOAD_BODY.as_bytes()).await;
    browser.close().await;
}
