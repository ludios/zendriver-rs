//! `BrowserBuilder::env` against real Chrome.
//!
//! Unix only: the test gives Chrome its time zone through `TZ`, which
//! Windows' ICU never reads (it asks Windows instead).

#![cfg(all(feature = "integration-tests", unix))]

use zendriver::Browser;
use zendriver_testkit::{eval, launch_for_test};

/// Chrome takes its time zone from `TZ`, so a page's time zone shows that
/// Chrome got the variable. Of two different zones, at least one differs
/// from the zone Chrome would have without `TZ`, so the test can't pass
/// without `env`.
#[tokio::test]
async fn chrome_gets_a_variable_set_with_env() {
    for tz in ["Pacific/Kiritimati", "America/St_Johns"] {
        let browser = launch_for_test(Browser::builder().env("TZ", tz)).await;
        let zone = eval(
            &browser.main_tab(),
            "Intl.DateTimeFormat().resolvedOptions().timeZone",
        )
        .await;
        browser.close().await;
        assert_eq!(zone.as_str(), Some(tz));
    }
}
