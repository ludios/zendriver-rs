//! The tab registry (`Browser::tabs`) against real Chrome and the fixture
//! server.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use zendriver::Browser;
use zendriver_testkit::{FixtureServer, SITE_A, launch_for_test, load};

/// The registry holds one tab per target, and the main tab's is the main
/// tab itself: Chrome attaches the main target a second time, under another
/// session id, which once made the registry a second `Tab` for it, with a
/// socket, trackers and interception of its own.
#[tokio::test]
async fn the_main_target_has_one_tab_the_main_tab() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let main = browser.main_tab();
    load(&main, &server.url(SITE_A, "/plain")).await;

    let tabs = browser.tabs().await;
    assert_eq!(tabs.len(), 1, "{} tabs", tabs.len());
    assert_eq!(tabs[0].target_id(), main.target_id());
    assert!(
        std::ptr::eq(tabs[0].session(), main.session()),
        "the registry's tab for the main target is another Tab than main_tab()"
    );
    browser.close().await;
}
