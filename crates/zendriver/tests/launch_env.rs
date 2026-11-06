//! `BrowserBuilder::env` against real Chrome.

#![cfg(feature = "integration-tests")]

use zendriver::Browser;
use zendriver_testkit::{eval, launch_for_test};

/// A time zone that a test machine is unlikely to be in, so that a page
/// reports it only if Chrome got it from `BrowserBuilder::env`.
const TZ: &str = "Pacific/Kiritimati";

/// Chrome takes its time zone from `TZ`, so a page's time zone shows that
/// Chrome got the variable.
#[tokio::test]
async fn chrome_gets_a_variable_set_with_env() {
    let browser = launch_for_test(Browser::builder().env("TZ", TZ)).await;
    let zone = eval(
        &browser.main_tab(),
        "Intl.DateTimeFormat().resolvedOptions().timeZone",
    )
    .await;
    browser.close().await;
    assert_eq!(zone.as_str(), Some(TZ));
}
