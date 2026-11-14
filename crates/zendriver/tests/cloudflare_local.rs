//! The Cloudflare bypass against a local imitation of Cloudflare's
//! full-page challenge, in real Chrome.
//!
//! The imitation has what the driver depends on, as seen on the live
//! challenge (Chrome 157, November 2026):
//!
//! - the widget's iframe is cross-site to the page, as
//!   `challenges.cloudflare.com` is, and sits in a *closed* shadow root,
//!   which no script in the page can enter;
//! - a hidden `cf-turnstile-response` input sits beside the shadow host, in
//!   the light DOM;
//! - once the checkbox is clicked, the page removes the widget, then the
//!   input, and only later navigates to the page it guards.
//!
//! Each test runs it in one of Chrome's two process layouts, and checks
//! that it got that layout: under Chrome's default site isolation the
//! widget runs in a process of its own, and with site isolation off, in the
//! page's process.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use serde_json::json;
use zendriver::{Browser, BrowserBuilder, ClearanceOutcome, Tab, TurnstileSelectors};
use zendriver_testkit::axum::Router;
use zendriver_testkit::axum::response::Html;
use zendriver_testkit::axum::routing::get;
use zendriver_testkit::{FixtureServer, SITE_A, assert_eventually, launch_for_test};

/// Path of the widget page, served from `b.test`. The tests' iframe marker.
const WIDGET_PATH: &str = "/fake-turnstile";

/// The challenge page, on `a.test`. Its widget is served from `b.test`.
const CHALLENGE: &str = r#"<!doctype html>
<title>Just a moment...</title>
<div><div id="host"></div><input type="hidden" name="cf-turnstile-response"></div>
<script>
  const root = document.getElementById("host").attachShadow({ mode: "closed" });
  const frame = document.createElement("iframe");
  frame.src = location.protocol + "//b.test:" + location.port + "/fake-turnstile";
  frame.style.cssText = "width: 300px; height: 65px; border: 0";
  root.appendChild(frame);
  addEventListener("message", (event) => {
    if (event.data !== "passed") return;
    document.getElementById("host").remove();
    setTimeout(() => document.querySelector("[name=cf-turnstile-response]").remove(), 300);
    setTimeout(() => location.replace("/guarded"), 1500);
  });
</script>"#;

/// The widget: a 23-pixel checkbox from 9 to 32 pixels in and centred
/// vertically, where the live widget has it, beside text that does nothing
/// when clicked. Checking the box tells the challenge page.
const WIDGET: &str = r#"<!doctype html>
<body style="margin: 0">
<input type="checkbox" id="box"
  style="position: absolute; left: 9px; top: 21px; width: 23px; height: 23px; margin: 0">
<span style="position: absolute; left: 44px; top: 22px">Verify you are human</span>
<script>
  document.getElementById("box").addEventListener("change", () => parent.postMessage("passed", "*"));
</script>"#;

/// The page the challenge guards.
const GUARDED: &str = "<!doctype html><title>Guarded page</title><p>guarded content</p>";

fn routes() -> Router {
    Router::new()
        .route("/challenge", get(|| async { Html(CHALLENGE) }))
        .route(WIDGET_PATH, get(|| async { Html(WIDGET) }))
        .route("/guarded", get(|| async { Html(GUARDED) }))
}

/// Whether Chrome lists the widget's frame as a target of its own, which it
/// does only for a frame running in a process of its own.
async fn widget_has_its_own_process(tab: &Tab) -> bool {
    let targets = tab
        .session()
        .call("Target.getTargets", json!({}))
        .await
        .unwrap();
    targets["targetInfos"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["type"] == "iframe" && t["url"].as_str().unwrap().contains(WIDGET_PATH))
}

/// Load the challenge in a Chrome launched from `builder`, check the
/// widget's process layout against `own_process`, then run the bypass and
/// check that it returns on the guarded page.
async fn clears_the_challenge(builder: BrowserBuilder, own_process: bool) {
    let server = FixtureServer::start_with(routes()).await;
    let browser = launch_for_test(builder).await;
    let tab = browser.main_tab();
    tab.goto(server.url(SITE_A, "/challenge")).await.unwrap();
    tab.wait_for_load().await.unwrap();

    let bypass = tab.cloudflare().selectors(TurnstileSelectors {
        iframe_src_contains: WIDGET_PATH.into(),
        ..TurnstileSelectors::default()
    });
    assert_eventually(Duration::from_secs(10), "the widget mounts", || async {
        bypass.is_challenge_present().await.unwrap()
    })
    .await;
    assert_eq!(widget_has_its_own_process(&tab).await, own_process);

    let outcome = bypass
        .wait_for_clearance(Duration::from_secs(20))
        .await
        .unwrap();
    assert!(
        matches!(outcome, ClearanceOutcome::ChallengeGone),
        "expected ChallengeGone, got {outcome:?}"
    );
    // No waiting here: the run is meant to end on the page the challenge
    // navigates to, not on the challenge page after its markers are gone.
    assert_eq!(tab.title().await.unwrap(), "Guarded page");
    browser.close().await;
}

#[tokio::test]
async fn clears_a_closed_shadow_root_widget_in_the_pages_process() {
    let shared = Browser::builder().arg("--disable-site-isolation-trials");
    clears_the_challenge(shared, false).await;
}

#[tokio::test]
async fn clears_a_closed_shadow_root_widget_in_a_process_of_its_own() {
    clears_the_challenge(Browser::builder(), true).await;
}
