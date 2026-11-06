//! The screen, window and viewport that a headless launch's pages see under
//! the `native` stealth profile (the default). Instead of patching the page,
//! it launches headless Chrome with a screen (`--screen-info`) and windows
//! that fill its work area (see `StealthObserver::headless_flags`), so
//! Chrome itself reports geometry that agrees.

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
/// `screen_size` (CSS pixels) at `device_pixel_ratio`, with a work area of
/// `work_area` at its top left, and a window that fills that work area,
/// whose viewport is as wide as the window and shorter (by the browser's
/// toolbars, which headless Chrome emulates).
fn assert_fills_work_area(
    seen: &Value,
    screen_size: [u64; 2],
    work_area: [u64; 2],
    device_pixel_ratio: f64,
) {
    let [avail_width, avail_height] = work_area;
    let expected = json!({
        "screen": screen_size,
        "workArea": [0, 0, avail_width, avail_height],
        "window": [0, 0, avail_width, avail_height],
        "ownScreenProperties": [],
    });
    let mut rest = seen.clone();
    let object = rest.as_object_mut().expect("an object");
    let viewport = object.remove("viewport").expect("a viewport");
    let ratio = object
        .remove("devicePixelRatio")
        .expect("a devicePixelRatio");
    assert_eq!(rest, expected, "all of it: {seen}");
    assert_eq!(ratio.as_f64(), Some(device_pixel_ratio), "{seen}");
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
        assert_fills_work_area(&seen, [1920, 1080], [1920, 1032], 1.0);
    }
}

/// Each window's tab still fills the work area once other windows have
/// opened, which is why `Browser::new_tab` tells callers to give each page
/// that they work in at the same time its own window.
#[tokio::test]
async fn a_window_keeps_its_geometry_while_others_open() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let mut tabs = vec![browser.main_tab()];
    for _ in 0..2 {
        tabs.push(browser.new_window().await.expect("new window"));
    }
    let mut seen = Vec::new();
    for tab in &tabs {
        seen.push(geometry(&server, tab).await);
    }
    browser.close().await;
    for seen in seen {
        assert_fills_work_area(&seen, [1920, 1080], [1920, 1032], 1.0);
    }
}

#[tokio::test]
#[ignore = "known bug: a background tab's page sees its viewport as its window \
            (at (0, 87), with outerHeight equal to innerHeight), because focus \
            emulation keeps the tab visible after Chrome detaches its view from \
            the window"]
async fn known_bug_a_background_tab_sees_its_viewport_as_its_window() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let background = browser.main_tab();
    browser.new_tab().await.expect("new tab");
    let seen = geometry(&server, &background).await;
    browser.close().await;
    assert_fills_work_area(&seen, [1920, 1080], [1920, 1032], 1.0);
}

/// A builder whose persona has the screen `screen` (the members of a
/// `ScreenSpec`, as JSON).
fn with_screen(screen: &str) -> BrowserBuilder {
    let persona: Persona = format!(r#"{{"screen": {{{screen}}}}}"#)
        .parse()
        .expect("persona JSON");
    Browser::builder().persona_overlay(persona)
}

#[tokio::test]
async fn a_configured_screen_reaches_chrome() {
    let builder = with_screen(
        r#""width": 1440, "height": 900, "device_pixel_ratio": 2.0,
           "avail_width": 1400, "avail_height": 860"#,
    );
    for seen in geometry_of_first_tab_and_new_window(builder).await {
        assert_fills_work_area(&seen, [1440, 900], [1400, 860], 2.0);
    }
}

/// At a fractional ratio, the sizes are exact as long as they are whole
/// physical pixels: 1536x864 at 1.25 is 1920x1080, and 2192x1232 at 1.75
/// (with the default taskbar of 48, which is 84 physical pixels) is
/// 3836x2156. At 1.75, Chrome's rounding would make the taskbar 49, and a
/// window 1596 wide 1597, without zendriver making up for it.
/// zendriver takes fractional ratios only on Linux.
#[tokio::test]
#[cfg(target_os = "linux")]
async fn a_screen_at_a_fractional_ratio_reaches_chrome_exactly() {
    let builder = with_screen(
        r#""width": 1536, "height": 864, "device_pixel_ratio": 1.25,
           "avail_width": 1536, "avail_height": 824"#,
    );
    for seen in geometry_of_first_tab_and_new_window(builder).await {
        assert_fills_work_area(&seen, [1536, 864], [1536, 824], 1.25);
    }
    let builder = with_screen(r#""width": 2192, "height": 1232, "device_pixel_ratio": 1.75"#);
    for seen in geometry_of_first_tab_and_new_window(builder).await {
        assert_fills_work_area(&seen, [2192, 1232], [2192, 1184], 1.75);
    }
    let builder = with_screen(
        r#""width": 1600, "height": 896, "device_pixel_ratio": 1.75,
           "avail_width": 1596, "avail_height": 848"#,
    );
    for seen in geometry_of_first_tab_and_new_window(builder).await {
        assert_fills_work_area(&seen, [1600, 896], [1596, 848], 1.75);
    }
}

/// zendriver leaves out its own screen switches then (and its window
/// switches, which describe its own screen).
#[tokio::test]
async fn the_callers_screen_info_wins() {
    let builder = Browser::builder().arg("--screen-info={1366x768 workAreaBottom=40}");
    for seen in geometry_of_first_tab_and_new_window(builder).await {
        assert_eq!(seen["screen"], json!([1366, 768]), "{seen}");
        assert_eq!(seen["workArea"], json!([0, 0, 1366, 728]), "{seen}");
    }
}

/// The window fills the work area without being maximized: Chrome would
/// ignore the size that the first `Browser.setWindowBounds` asks a maximized
/// window for, and only restore it to a default size.
#[tokio::test]
async fn set_window_size_takes_effect() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    tab.set_window_size(1280, 720)
        .await
        .expect("set_window_size");
    let seen = geometry(&server, &tab).await;
    browser.close().await;
    assert_eq!(seen["window"], json!([0, 0, 1280, 720]), "{seen}");
}
