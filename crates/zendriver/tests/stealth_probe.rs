//! The stealth consistency probe under each stealth profile: the identity a
//! page reads in its main world, a same-origin iframe and a dedicated worker
//! must match each other and the HTTP headers.
//!
//! Fields and sources are listed on [`disagreements`]. A disagreement that
//! is a known, open bug is listed in [`KNOWN_BUGS`]: the test fails on any
//! disagreement that isn't listed, and on any listed one that's gone (fixed,
//! so delete its row).
//!
//! Linux only: the known disagreements depend on the host OS (a Win32
//! persona on a Linux host leaks Linux), and CI runs these on Linux.

#![cfg(all(feature = "integration-tests", target_os = "linux"))]
#![allow(clippy::panic)]

use std::collections::BTreeSet;

use zendriver::stealth::{Platform, StealthProfile};
use zendriver::{Browser, BrowserBuilder};
use zendriver_testkit::{
    FixtureServer, PROBE_JS, PROBE_PATH, describe_disagreements, disagreements, eval,
    launch_for_test, load,
};

/// The TODO item for workers, which no stealth patch reaches: they report
/// the real GPU (SwiftShader, since workers get no GPU) and memory.
const WORKER_UNPATCHED: &str =
    "P1 Stealth: worker GPU name and deviceMemory differ from the page under spoofed (R6)";

/// The TODO item for the JS patches and the CDP overrides taking the
/// identity from different sources: the UA, the headers and the (unpatched)
/// worker say Windows, the page's `navigator` says Linux.
const TWO_IDENTITIES: &str =
    "P1 Stealth: JS-visible and HTTP identity come from different sources (ST #1, R6)";

/// `(field, profiles, why)`: a field that disagrees under each of
/// `profiles` because of an open bug, named by its TODO item. Recorded on
/// Linux with Chrome 156.
const KNOWN_BUGS: &[(&str, &[&str], &str)] = &[
    (
        "deviceMemory",
        &["spoofed", "spoofed win32"],
        WORKER_UNPATCHED,
    ),
    ("webgl", &["spoofed", "spoofed win32"], WORKER_UNPATCHED),
    ("os", &["spoofed win32"], TWO_IDENTITIES),
    ("uaPlatform", &["spoofed win32"], TWO_IDENTITIES),
    ("platformVersion", &["spoofed win32"], TWO_IDENTITIES),
];

/// Load the probe in a browser launched from `builder`, and check that its
/// disagreements are exactly `profile`'s rows in [`KNOWN_BUGS`].
async fn check(profile: &str, builder: BrowserBuilder) {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(builder).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", PROBE_PATH)).await;
    let probe = eval(&tab, PROBE_JS).await;
    let found = disagreements(&probe, &server.requests());
    browser.close().await;

    let known: BTreeSet<&str> = KNOWN_BUGS
        .iter()
        .filter(|(_, profiles, _)| profiles.contains(&profile))
        .map(|(field, ..)| *field)
        .collect();
    let unexpected: BTreeSet<&str> = found
        .keys()
        .copied()
        .filter(|f| !known.contains(f))
        .collect();
    let fixed: BTreeSet<&str> = known
        .iter()
        .copied()
        .filter(|f| !found.contains_key(f))
        .collect();
    assert!(
        unexpected.is_empty() && fixed.is_empty(),
        "under {profile}: disagreeing fields not in KNOWN_BUGS: {unexpected:?}; fields in \
         KNOWN_BUGS that now agree (delete their rows): {fixed:?}\n\nAll disagreements:\n{}\n\
         Probe: {probe:#}",
        describe_disagreements(&found),
    );
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
    check(
        "spoofed",
        Browser::builder().stealth(StealthProfile::spoofed()),
    )
    .await;
}

/// The book's example, and what MCP's `spoof_windows` launches.
#[tokio::test]
async fn probe_agrees_under_spoofed_win32() {
    let profile = StealthProfile::spoofed().platform(Platform::Win32);
    check("spoofed win32", Browser::builder().stealth(profile)).await;
}
