//! What a headless launch's pages see of the pointing devices, through the
//! CSS media features `pointer`, `hover`, `any-pointer` and `any-hover`, and
//! through `navigator.maxTouchPoints`.
//!
//! Linux only: headless Chrome on Linux has no input devices, and the
//! `native` and `spoofed` stealth profiles give its pages a mouse with
//! `--blink-settings` (see `StealthProfile::pointer_blink_settings`).
//! Elsewhere, Chrome gives pages a pointer of its own.

#![cfg(all(feature = "integration-tests", target_os = "linux"))]

use serde_json::{Value, json};
use zendriver::Browser;
use zendriver::stealth::StealthProfile;
use zendriver_testkit::axum::Router;
use zendriver_testkit::axum::response::Html;
use zendriver_testkit::axum::routing::get;
use zendriver_testkit::{FixtureServer, SITE_A, SITE_B, TestBrowser, eval, launch_for_test, load};

/// JS that evaluates to what the page sees of its pointing devices: for each
/// media feature, the values that it matches (space-separated), both through
/// `matchMedia` and through CSS `@media` rules, since detectors use both;
/// plus `maxTouchPoints`, and whether `window` has `ontouchstart`.
const PROBE_JS: &str = r#"(() => {
  const features = {
    'pointer': ['none', 'coarse', 'fine'],
    'hover': ['none', 'hover'],
    'any-pointer': ['none', 'coarse', 'fine'],
    'any-hover': ['none', 'hover'],
  };
  const style = document.createElement('style');
  document.head.append(style);
  for (const [feature, values] of Object.entries(features)) {
    for (const value of values) {
      style.sheet.insertRule(`@media (${feature}: ${value}) { :root { --${feature}-${value}: 1 } }`);
    }
  }
  const computed = getComputedStyle(document.documentElement);
  const media = {};
  const css = {};
  for (const [feature, values] of Object.entries(features)) {
    media[feature] = values.filter(v => matchMedia(`(${feature}: ${v})`).matches).join(' ');
    css[feature] = values.filter(v => computed.getPropertyValue(`--${feature}-${v}`) !== '').join(' ');
  }
  style.remove();
  return {media, css, maxTouchPoints: navigator.maxTouchPoints, ontouchstart: 'ontouchstart' in window};
})()"#;

/// What [`PROBE_JS`] evaluates to when the page sees one kind of pointer,
/// `pointer` (`"fine"` or `"coarse"`), or no pointer (`"none"`); when the
/// pointer can hover (`hover` is `"hover"`) or not (`"none"`); and when the
/// page sees no touchscreen.
fn seen(pointer: &str, hover: &str) -> Value {
    let features = json!({
        "pointer": pointer,
        "hover": hover,
        "any-pointer": pointer,
        "any-hover": hover,
    });
    json!({"media": features, "css": features, "maxTouchPoints": 0, "ontouchstart": false})
}

/// A page that posts [`PROBE_JS`]'s result to its parent.
const PROBE_PAGE: &str = "/pointer-probe";

/// A fixture server that also serves [`PROBE_PAGE`].
async fn server_with_probe_page() -> FixtureServer {
    let page = format!("<!doctype html><script>parent.postMessage({PROBE_JS}, '*')</script>");
    let probe = Router::new().route(PROBE_PAGE, get(|| async { Html(page) }));
    FixtureServer::start_with(probe).await
}

/// JS that embeds an iframe of `url` and resolves to the first message that
/// the page receives.
fn embed_js(url: &str) -> String {
    let url = serde_json::to_string(url).expect("a string serializes");
    format!(
        "new Promise(resolve => {{
           addEventListener('message', e => resolve(e.data), {{once: true}});
           const frame = document.createElement('iframe');
           frame.src = {url};
           document.body.append(frame);
         }})"
    )
}

/// Whether Chrome runs a frame whose URL contains `url_part` out of
/// process, in a renderer of its own. Chrome then lists the frame as an
/// `iframe` target.
async fn is_out_of_process(browser: &TestBrowser, url_part: &str) -> bool {
    let targets = browser
        .cdp()
        .call_raw("Target.getTargets", json!({}), None)
        .await
        .expect("Target.getTargets");
    targets["targetInfos"]
        .as_array()
        .expect("targetInfos is an array")
        .iter()
        .any(|t| t["type"] == "iframe" && t["url"].as_str().is_some_and(|u| u.contains(url_part)))
}

/// The main frame and a cross-site iframe see a mouse. Chrome runs the
/// iframe in a renderer process of its own, which needs the settings too.
#[tokio::test]
async fn headless_pages_and_cross_site_iframes_see_a_mouse() {
    let server = server_with_probe_page().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    let in_main = eval(&tab, PROBE_JS).await;
    let in_child = eval(&tab, &embed_js(&server.url(SITE_B, PROBE_PAGE))).await;
    let out_of_process = is_out_of_process(&browser, SITE_B).await;
    browser.close().await;

    let mouse = seen("fine", "hover");
    assert_eq!(in_main, mouse, "main frame");
    assert_eq!(in_child, mouse, "cross-site iframe");
    assert!(
        out_of_process,
        "the iframe shared its parent's renderer, so it showed nothing about other renderers"
    );
}

/// With `StealthProfile::off()`, zendriver gives Chrome no Blink settings,
/// so the pages of headless Chrome see no pointing device. This also shows
/// that the probe can tell the difference.
#[tokio::test]
async fn headless_pages_see_no_pointer_with_stealth_off() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder().stealth(StealthProfile::off())).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    let seen_off = eval(&tab, PROBE_JS).await;
    browser.close().await;
    assert_eq!(seen_off, seen("none", "none"));
}

/// zendriver's settings join those of the caller's last `--blink-settings`
/// (the only one Chrome reads) instead of replacing them, and the caller's
/// win where both name a setting. Here the last switch says that no pointer
/// can hover, while zendriver's settings still make the primary pointer a
/// fine one that can hover. The earlier switch, which says that the only
/// pointer is coarse, is ignored, as Chrome would ignore it without
/// zendriver.
#[tokio::test]
async fn callers_last_blink_settings_join_zendrivers() {
    let server = FixtureServer::start().await;
    let builder = Browser::builder()
        .arg("--blink-settings=availablePointerTypes=2")
        .arg("--blink-settings=availableHoverTypes=1");
    let browser = launch_for_test(builder).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    let seen_mixed = eval(&tab, PROBE_JS).await;
    browser.close().await;

    let features = json!({
        "pointer": "fine",
        "hover": "hover",
        "any-pointer": "fine",
        "any-hover": "none",
    });
    assert_eq!(seen_mixed["media"], features);
    assert_eq!(seen_mixed["css"], features);
}
