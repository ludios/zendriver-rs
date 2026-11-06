//! The screen, window and viewport that a headless launch's pages see under
//! the `native` stealth profile (the default). Instead of patching the page,
//! it launches headless Chrome with a screen (`--screen-info`) and maximized
//! windows (see `StealthObserver::headless_flags`), so Chrome itself reports
//! geometry that agrees.

#![cfg(feature = "integration-tests")]

use serde_json::{Value, json};
use zendriver::{Browser, BrowserBuilder, Persona, Tab};
use zendriver_testkit::{FixtureServer, eval, launch_for_test, load};

/// JS that evaluates to what the page sees of its screen, its window and its
/// viewport.
const GEOMETRY_JS: &str = r#"({
  screen: [screen.width, screen.height],
  workArea: [screen.availLeft, screen.availTop, screen.availWidth, screen.availHeight],
  window: [screenX, screenY, outerWidth, outerHeight],
  viewport: [innerWidth, innerHeight],
  devicePixelRatio,
  ownScreenProperties: Object.getOwnPropertyNames(screen),
})"#;

/// Load a page in `tab` and return what [`GEOMETRY_JS`] evaluates to there.
async fn geometry(server: &FixtureServer, tab: &Tab) -> Value {
    load(tab, &server.url("127.0.0.1", "/plain")).await;
    eval(tab, GEOMETRY_JS).await
}

/// Assert that `seen` (from [`geometry`]) describes a screen of
/// `screen_size` (CSS pixels) at a whole `device_pixel_ratio`, with a work
/// area of `work_area` at its top left, and a window maximized to that work
/// area whose viewport is as wide as the window and shorter (by the
/// browser's toolbars, which headless Chrome emulates).
fn assert_maximized_on(
    seen: &Value,
    screen_size: [u64; 2],
    work_area: [u64; 2],
    device_pixel_ratio: u64,
) {
    let [avail_width, avail_height] = work_area;
    let expected = json!({
        "screen": screen_size,
        "workArea": [0, 0, avail_width, avail_height],
        "window": [0, 0, avail_width, avail_height],
        "devicePixelRatio": device_pixel_ratio,
        "ownScreenProperties": [],
    });
    let mut without_viewport = seen.clone();
    let viewport = without_viewport
        .as_object_mut()
        .and_then(|o| o.remove("viewport"))
        .expect("a viewport");
    assert_eq!(without_viewport, expected, "all of it: {seen}");
    assert_eq!(viewport[0], avail_width, "viewport width: {seen}");
    let viewport_height = viewport[1].as_u64().expect("a viewport height");
    assert!(
        0 < viewport_height && viewport_height < avail_height,
        "viewport height: {seen}"
    );
}

/// Launch `builder` headless, and return the geometry seen in its first tab
/// and in a new window.
async fn geometry_of_first_tab_and_new_window(builder: BrowserBuilder) -> [Value; 2] {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(builder).await;
    let first = geometry(&server, &browser.main_tab()).await;
    let window = browser.new_window().await.expect("new window");
    let second = geometry(&server, &window).await;
    browser.close().await;
    [first, second]
}

#[tokio::test]
async fn the_default_screen_is_1920x1080_with_a_taskbar() {
    for seen in geometry_of_first_tab_and_new_window(Browser::builder()).await {
        assert_maximized_on(&seen, [1920, 1080], [1920, 1032], 1);
    }
}

#[tokio::test]
async fn a_configured_screen_reaches_chrome() {
    let persona: Persona = r#"{"screen": {
        "width": 1440, "height": 900, "device_pixel_ratio": 2.0,
        "avail_width": 1400, "avail_height": 860
    }}"#
    .parse()
    .expect("persona JSON");
    let builder = Browser::builder().persona_overlay(persona);
    for seen in geometry_of_first_tab_and_new_window(builder).await {
        assert_maximized_on(&seen, [1440, 900], [1400, 860], 2);
    }
}

/// Chrome keeps the last of several `--screen-info` switches, and zendriver
/// leaves its own out.
#[tokio::test]
async fn the_callers_screen_info_wins() {
    let builder = Browser::builder().arg("--screen-info={1366x768 workAreaBottom=40}");
    for seen in geometry_of_first_tab_and_new_window(builder).await {
        assert_maximized_on(&seen, [1366, 768], [1366, 728], 1);
    }
}
