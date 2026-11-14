//! The stealth consistency probe under each stealth profile: the identity a
//! page reads in its main world, a same-origin iframe and a dedicated worker
//! must match each other and the HTTP headers.
//!
//! Fields and sources are listed on [`disagreements`]. A disagreement that
//! is a known, open bug is listed in
//! [`KNOWN_DISAGREEMENTS`](zendriver_testkit::KNOWN_DISAGREEMENTS): the test
//! fails on any disagreement that isn't listed, and on any listed one that's
//! gone (fixed, so delete its row).
//!
//! Linux only: the known disagreements depend on the host OS (a Win32
//! persona on a Linux host leaks Linux), and CI runs these on Linux.

#![cfg(all(feature = "integration-tests", target_os = "linux"))]
#![allow(clippy::panic)]

use serde_json::{Value, json};
use zendriver::stealth::{Platform, StealthProfile};
use zendriver::{Browser, BrowserBuilder, Persona};
use zendriver_testkit::{
    FixtureServer, PINNED_HARDWARE, PROBE_JS, PROBE_PATH, check_known_disagreements, disagreements,
    eval, launch_for_test, load,
};

/// Load the probe in a browser launched from `builder`, and check that its
/// disagreements are exactly `profile`'s known ones. Returns the probe's
/// readings (see [`PROBE_JS`]).
async fn check(profile: &str, builder: BrowserBuilder) -> Value {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(builder).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", PROBE_PATH)).await;
    let probe = eval(&tab, PROBE_JS).await;
    let found = disagreements(&probe, &server.requests());
    browser.close().await;

    if let Err(mismatch) = check_known_disagreements(profile, &found, &[]) {
        panic!("{mismatch}\nProbe: {probe:#}");
    }
    probe
}

/// A persona's language differs from the one that Chrome starts with, so it
/// shows where the persona's doesn't reach. The override gives it to pages
/// and iframes only: the worker reads Chrome's own list, and its requests
/// carry that list, so a launch has to give Chrome the persona's list too.
#[tokio::test]
async fn probe_agrees_with_a_personas_language() {
    let german: Persona = r#"{"locale": "de-DE"}"#.parse().expect("persona JSON");
    let probe = check("native", Browser::builder().persona_overlay(german)).await;
    // Agreeing on Chrome's own language would be no proof.
    assert_eq!(probe["worker"]["languages"], json!(["de-DE", "de"]));
}

/// `profile` with the page's hardware pinned (see [`PINNED_HARDWARE`]).
fn spoofed(profile: StealthProfile) -> BrowserBuilder {
    let pinned: Persona = PINNED_HARDWARE.parse().expect("persona JSON");
    Browser::builder().stealth(profile).persona_overlay(pinned)
}

#[tokio::test]
async fn probe_agrees_with_stealth_off() {
    check("off", Browser::builder().stealth(StealthProfile::off())).await;
}

#[tokio::test]
async fn probe_agrees_by_default() {
    check("default", Browser::builder()).await;
}

#[tokio::test]
async fn probe_agrees_under_native() {
    check(
        "native",
        Browser::builder().stealth(StealthProfile::native()),
    )
    .await;
}

#[tokio::test]
async fn probe_agrees_under_spoofed() {
    check("spoofed", spoofed(StealthProfile::spoofed())).await;
}

/// The book's example, and what MCP's `spoof_windows` launches.
#[tokio::test]
async fn probe_agrees_under_spoofed_win32() {
    let profile = StealthProfile::spoofed().platform(Platform::Win32);
    check("spoofed win32", spoofed(profile)).await;
}
