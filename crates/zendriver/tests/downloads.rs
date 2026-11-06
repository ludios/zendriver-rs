//! Downloads against real Chrome: where they go, and how long zendriver
//! can follow them.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use std::path::Path;
use std::time::Duration;

use zendriver::{Browser, DownloadProgressState, Tab};
use zendriver_testkit::{
    DOWNLOAD_BODY, FixtureServer, SITE_A, assert_eventually, eval, launch_for_test, load,
    trickle_body,
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

/// A download goes on after its tab closes, and zendriver follows it to its
/// end: the tab here has its own socket, which closing it shuts down, and
/// the download is still arriving when it closes.
#[tokio::test]
async fn a_download_is_saved_after_its_tab_closes() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.new_tab().await.unwrap();
    assert!(tab.session().is_root(), "the tab has no socket of its own");
    load(&tab, &server.url(SITE_A, "/plain")).await;
    let expectation = tab.expect_download().await.unwrap();
    // About 2 s of download.
    let url = "/trickle?chunks=20&chunk_bytes=1024&interval_ms=100&attachment=slow.bin";
    eval(&tab, &format!("location.href = {url:?}")).await;
    let dl = expectation.await.unwrap();
    assert_eq!(dl.suggested_filename(), "slow.bin");
    assert_eq!(dl.state().state, DownloadProgressState::InProgress);

    tab.close().await.unwrap();
    let dest = tempfile::tempdir().unwrap();
    let saved = dest.path().join("slow.bin");
    tokio::time::timeout(PROMPTLY, dl.save_to(&saved))
        .await
        .expect("the download didn't end")
        .unwrap();
    assert_eq!(
        tokio::fs::read(&saved).await.unwrap(),
        trickle_body(20, 1024)
    );
    browser.close().await;
}

/// In a context whose downloads go to a directory the caller chose,
/// `expect_download` leaves them there, under Chrome's name, and finds the
/// file where Chrome says it is.
#[tokio::test]
async fn a_download_into_a_chosen_directory_is_found_there() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let dir = tempfile::tempdir().unwrap();
    let tab = browser.main_tab();
    tab.set_download_path(dir.path()).await.unwrap();
    load(&tab, &server.url(SITE_A, "/downloads")).await;

    let expectation = tab.expect_download().await.unwrap();
    click(&tab, "#download").await;
    let dl = expectation.await.unwrap();
    assert_eq!(dl.download_dir(), dir.path());
    let file = tokio::time::timeout(PROMPTLY, dl.wait())
        .await
        .expect("the download didn't end")
        .unwrap();
    assert_eq!(file, dir.path().join("report.csv"));
    assert_eq!(
        tokio::fs::read(&file).await.unwrap(),
        DOWNLOAD_BODY.as_bytes()
    );
    browser.close().await;
}

/// `download_file` keeps the setting `expect_download` made for the
/// context, so the expectation catches the file `download_file` fetched
/// (before, `download_file` sent it to `<cwd>/downloads`, where
/// `save_to` didn't look).
#[tokio::test]
async fn expect_download_catches_what_download_file_fetches() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;

    let expectation = tab.expect_download().await.unwrap();
    let url = server.url(SITE_A, "/download");
    tab.download_file(url, Some("fetched.csv".into()))
        .await
        .unwrap();
    let dl = expectation.await.unwrap();
    assert_eq!(dl.suggested_filename(), "fetched.csv");
    let dest = tempfile::tempdir().unwrap();
    let saved = dest.path().join("fetched.csv");
    tokio::time::timeout(PROMPTLY, dl.save_to(&saved))
        .await
        .expect("the download didn't end")
        .unwrap();
    assert_eq!(
        tokio::fs::read(&saved).await.unwrap(),
        DOWNLOAD_BODY.as_bytes()
    );

    // Another tab of the same context shares its setting.
    let other = browser.new_tab().await.unwrap();
    load(&other, &server.url(SITE_A, "/downloads")).await;
    let expectation = other.expect_download().await.unwrap();
    click(&other, "#download").await;
    let dl = expectation.await.unwrap();
    let file = tokio::time::timeout(PROMPTLY, dl.wait())
        .await
        .expect("the download didn't end")
        .unwrap();
    assert_eq!(
        tokio::fs::read(&file).await.unwrap(),
        DOWNLOAD_BODY.as_bytes()
    );
    browser.close().await;
}

/// A tiny download can complete before its tab's `Page.downloadWillBegin`
/// is processed, since the browser's progress comes on another socket; the
/// claim must still find how it ended. Twenty rounds give the race room.
#[tokio::test]
async fn a_tiny_download_is_found_however_the_events_race() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/downloads")).await;
    for round in 0..20 {
        let expectation = tab.expect_download().await.unwrap();
        click(&tab, "#download-tiny").await;
        let dl = expectation.await.unwrap();
        let file = tokio::time::timeout(PROMPTLY, dl.wait())
            .await
            .unwrap_or_else(|_| panic!("round {round}: the download didn't end"))
            .unwrap();
        assert_eq!(tokio::fs::read(&file).await.unwrap(), b"x", "round {round}");
    }
    browser.close().await;
}
