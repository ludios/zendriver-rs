//! `BrowserBuilder::env` and `BrowserBuilder::env_remove` against real
//! Chrome.
//!
//! Unix only: the tests give Chrome its time zone through `TZ`, which
//! Windows' ICU never reads (it asks Windows instead).

#![cfg(all(feature = "integration-tests", unix))]

use zendriver::{Browser, BrowserBuilder};
use zendriver_testkit::{eval, launch_for_test};

/// Two different time zones. At least one of them differs from the zone
/// that Chrome has without `TZ`.
const ZONES: [&str; 2] = ["Pacific/Kiritimati", "America/St_Johns"];

/// Launch `builder` and return the time zone that its page reports.
async fn page_zone(builder: BrowserBuilder) -> String {
    let browser = launch_for_test(builder).await;
    let zone = eval(
        &browser.main_tab(),
        "Intl.DateTimeFormat().resolvedOptions().timeZone",
    )
    .await;
    browser.close().await;
    zone.as_str()
        .expect("the time zone is a string")
        .to_string()
}

/// Chrome takes its time zone from `TZ`, so a page's time zone shows that
/// Chrome got the variable. Because one of `ZONES` differs from the zone
/// Chrome has without `TZ`, the test can't pass without `env`.
#[tokio::test]
async fn chrome_gets_a_variable_set_with_env() {
    for tz in ZONES {
        assert_eq!(page_zone(Browser::builder().env("TZ", tz)).await, tz);
    }
}

/// `env_remove` after `env` keeps `TZ` away from Chrome, so both launches
/// report the same zone (the one Chrome has without `TZ`), though `env` named
/// a different zone for each.
#[tokio::test]
async fn env_remove_undoes_an_earlier_env() {
    let mut zones = Vec::new();
    for tz in ZONES {
        zones.push(page_zone(Browser::builder().env("TZ", tz).env_remove("TZ")).await);
    }
    assert_eq!(zones[0], zones[1]);
}
