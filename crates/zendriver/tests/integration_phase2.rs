//! Phase 2 end-to-end stealth tests against real Chrome + wiremock.

#![cfg(feature = "integration-tests")]

use serde_json::json;
use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zendriver::stealth::StealthProfile;
use zendriver::{Browser, Persona};
use zendriver_testkit::axum::Router;
use zendriver_testkit::axum::http::header::CONTENT_SECURITY_POLICY;
use zendriver_testkit::axum::response::Html;
use zendriver_testkit::axum::routing::get;
use zendriver_testkit::{FixtureServer, PINNED_HARDWARE, SITE_A, eval, launch_for_test, load};

async fn fixture_with_html(html: &str) -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(html.as_bytes().to_vec(), "text/html"),
        )
        .mount(&mock)
        .await;
    mock
}

#[tokio::test]
#[serial]
async fn spoofed_profile_hides_navigator_webdriver_via_flag() {
    let mock = fixture_with_html("<!doctype html><body>hello</body>").await;
    let browser = Browser::builder()
        .stealth(StealthProfile::spoofed())
        .launch()
        .await
        .unwrap();
    let tab = browser.main_tab();
    tab.goto(&mock.uri()).await.unwrap();
    tab.wait_for_load().await.unwrap();
    let wd: bool = tab.evaluate_main("navigator.webdriver").await.unwrap();
    assert!(
        !wd,
        "spoofed profile hides webdriver via the AutomationControlled flag"
    );
    browser.close().await.unwrap();
}

/// Under `spoofed`, the bootstrap installs on a page whose Content Security
/// Policy forbids every script, with no `Page.setBypassCSP`: Chrome runs it
/// through DevTools, which CSP doesn't restrict. The page's own script stays
/// blocked, which shows that the policy is in force.
#[tokio::test]
#[serial]
async fn spoofed_bootstrap_installs_under_a_csp_that_forbids_every_script() {
    let page = Router::new().route(
        "/strict-csp",
        get(|| async {
            (
                [(CONTENT_SECURITY_POLICY, "default-src 'none'")],
                Html(
                    "<!doctype html><title>before</title>\
                     <script>document.title = 'page script ran'</script>",
                ),
            )
        }),
    );
    let server = FixtureServer::start_with(page).await;
    let pinned: Persona = PINNED_HARDWARE.parse().expect("persona JSON");
    let builder = Browser::builder()
        .stealth(StealthProfile::spoofed())
        .persona_overlay(pinned);
    let browser = launch_for_test(builder).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/strict-csp")).await;
    let seen = eval(&tab, "[document.title, navigator.deviceMemory]").await;
    browser.close().await;
    assert_eq!(
        seen,
        json!(["before", 1]),
        "[title, deviceMemory]: the CSP must block the page's script, and the \
         bootstrap must still report PINNED_HARDWARE's deviceMemory"
    );
}

/// A `StealthProfile`'s hardware pins reach the page when no persona pins
/// other values: the base persona of a launch used to pin the host's.
#[tokio::test]
#[serial]
async fn spoofed_profile_hardware_pins_reach_the_page() {
    let server = FixtureServer::start().await;
    let profile = StealthProfile::spoofed().cpu_count(3).memory_gb(2);
    let browser = launch_for_test(Browser::builder().stealth(profile)).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/plain")).await;
    let seen = eval(
        &tab,
        "[navigator.hardwareConcurrency, navigator.deviceMemory]",
    )
    .await;
    browser.close().await;
    assert_eq!(seen, json!([3, 2]), "[hardwareConcurrency, deviceMemory]");
}

/// Chrome defines `navigator.deviceMemory` only in secure contexts, so the
/// `spoofed` bootstrap mustn't define it on a plain-HTTP page.
#[tokio::test]
#[serial]
async fn spoofed_defines_no_device_memory_in_an_insecure_context() {
    let server = FixtureServer::start().await;
    let pinned: Persona = PINNED_HARDWARE.parse().expect("persona JSON");
    let builder = Browser::builder()
        .stealth(StealthProfile::spoofed())
        .persona_overlay(pinned);
    let browser = launch_for_test(builder).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    let seen = eval(&tab, "[isSecureContext, 'deviceMemory' in navigator]").await;
    browser.close().await;
    assert_eq!(
        seen,
        json!([false, false]),
        "[isSecureContext, has deviceMemory]"
    );
}

#[tokio::test]
#[serial]
async fn native_profile_hides_webdriver_via_flag() {
    // Native applies `--disable-blink-features=AutomationControlled` (a launch
    // flag, NOT a prototype patch), so `navigator.webdriver` reports false while
    // native still injects no JS. Previously the flag lived only in the Spoofed
    // branch, so native leaked `navigator.webdriver === true` — an instant bot
    // tell.
    let mock = fixture_with_html("<!doctype html><body>hello</body>").await;
    let browser = Browser::builder()
        .stealth(StealthProfile::native())
        .launch()
        .await
        .unwrap();
    let tab = browser.main_tab();
    tab.goto(&mock.uri()).await.unwrap();
    tab.wait_for_load().await.unwrap();
    let wd: bool = tab.evaluate_main("navigator.webdriver").await.unwrap();
    assert!(
        !wd,
        "native profile hides webdriver via the AutomationControlled flag"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
#[serial]
async fn ua_string_no_longer_contains_headless_under_native() {
    let mock = fixture_with_html("<!doctype html><body>hello</body>").await;
    let browser = Browser::builder()
        .stealth(StealthProfile::native())
        .launch()
        .await
        .unwrap();
    let tab = browser.main_tab();
    tab.goto(&mock.uri()).await.unwrap();
    tab.wait_for_load().await.unwrap();
    let ua: String = tab.evaluate_main("navigator.userAgent").await.unwrap();
    assert!(!ua.contains("HeadlessChrome"), "got: {ua}");
    browser.close().await.unwrap();
}

#[tokio::test]
#[serial]
async fn isolated_world_eval_does_not_see_page_globals() {
    let mock = fixture_with_html(
        r#"
        <!doctype html><script>window.evil = "should not be visible";</script>
    "#,
    )
    .await;
    let browser = Browser::builder()
        .stealth(StealthProfile::off())
        .launch()
        .await
        .unwrap();
    let tab = browser.main_tab();
    tab.goto(&mock.uri()).await.unwrap();
    tab.wait_for_load().await.unwrap();
    let v: Option<String> = tab
        .evaluate("typeof window.evil === 'undefined' ? null : window.evil")
        .await
        .unwrap();
    assert_eq!(v, None, "isolated world should NOT see window.evil");
    let v: String = tab.evaluate_main("window.evil").await.unwrap();
    assert_eq!(
        v, "should not be visible",
        "main world DOES see window.evil"
    );
    browser.close().await.unwrap();
}

/// Under `spoofed`, `navigator.cpuPerformance` follows the persona's CPU
/// count instead of the host's processors: Chrome rates 1 processor as the
/// low tier, 1.
#[tokio::test]
async fn spoofed_cpu_performance_follows_the_personas_cpu_count() {
    let server = FixtureServer::start().await;
    let pinned: Persona = PINNED_HARDWARE.parse().expect("persona JSON");
    let builder = Browser::builder()
        .stealth(StealthProfile::spoofed())
        .persona_overlay(pinned);
    let browser = launch_for_test(builder).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/plain")).await;
    let seen = eval(
        &tab,
        "[navigator.hardwareConcurrency, navigator.cpuPerformance]",
    )
    .await;
    browser.close().await;
    assert_eq!(seen, json!([1, 1]));
}
