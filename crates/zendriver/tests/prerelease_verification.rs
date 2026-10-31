//! v0.1.0 pre-release regression locks for upstream Python zendriver bugs.
//!
//! Each test in this file pins a behavior that broke in the Python port and
//! that the Rust port's architecture is meant to make impossible. They are
//! verification tests, not feature tests — if they regress, the architectural
//! win has been lost and the fix belongs at the design layer, not by relaxing
//! the assertion.
//!
//! Gated behind `integration-tests` because each test launches real Chrome.
//!
//! # This file is the windows-latest CI allowlist
//!
//! `ci.yml`'s `test-integration` job runs `-E
//! 'binary_id(zendriver::prerelease_verification)'` on Windows — this binary
//! and nothing else. It qualifies because every test here is network-free:
//! `about:blank` and `evaluate`, no fixture server.
//!
//! That allowlist was originally justified by a belief that Chrome on the
//! runner cannot complete a loopback fetch, so only `wiremock`-backed tests
//! hung. That was wrong: all three tests below then timed out at 360s too,
//! silently. The real cause was `zendriver-stealth`'s Chrome version probe
//! blocking forever on `chrome.exe --version` inside `launch()` — see
//! `fingerprint::probe_chrome_version`. Loopback on the runner remains
//! **unverified** rather than known-broken.
//!
//! **Prefer not to add a `wiremock` / `MockServer` fixture, or any test that
//! fetches a URL, to this file** while the Windows leg is filtered to it: such
//! a test belongs in an `integration_phase*` binary, which Windows does not
//! run. If the leg is ever widened to `kind(test)`, this caveat retires.
//!
//! Coverage map:
//! - `ua_override_persists_across_new_tabs` → cdpdriver/zendriver#107
//! - `repeated_launch_close_leaves_no_orphan_chrome_processes` → #33, #198
//! - `browser_close_during_inflight_cdp_calls_does_not_hang` → #89

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use serial_test::serial;
use zendriver::Browser;
use zendriver::stealth::StealthProfile;
use zendriver_testkit::launch_for_test;

/// cdpdriver/zendriver#107: in headless mode, a UA override set on the main
/// tab failed to propagate to tabs opened after launch. The Rust port wires
/// the UA override through `StealthObserver`, which attaches per-target — so
/// every new tab should see the same UA without any per-tab re-application
/// by the caller.
#[tokio::test]
#[serial]
async fn ua_override_persists_across_new_tabs() {
    const UA: &str = "ZendriverPortUA/0.1";

    let browser = Browser::builder()
        .headless(true)
        .stealth(StealthProfile::native().user_agent(UA))
        .launch()
        .await
        .expect("launch");

    let main = browser.main_tab();
    let main_ua: String = main
        .evaluate("navigator.userAgent")
        .await
        .expect("read UA on main tab");
    assert_eq!(
        main_ua, UA,
        "main tab must report the overridden UA, got {main_ua:?}"
    );

    for i in 0..3 {
        let tab = browser
            .new_tab()
            .await
            .unwrap_or_else(|e| panic!("open new tab #{i}: {e}"));
        let ua: String = tab
            .evaluate("navigator.userAgent")
            .await
            .unwrap_or_else(|e| panic!("read UA on new tab #{i}: {e}"));
        assert_eq!(
            ua, UA,
            "new tab #{i} must inherit overridden UA, got {ua:?}"
        );
    }

    browser.close().await.expect("close");
}

/// Parse the Chrome major out of a UA string; handles both `Chrome/150.0.x.y`
/// and headless's `HeadlessChrome/150.0.x.y` (the latter contains the former).
fn chrome_major_from_ua(ua: &str) -> u32 {
    let rest = ua
        .find("Chrome/")
        .map(|i| &ua[i + "Chrome/".len()..])
        .unwrap_or_else(|| panic!("no `Chrome/` token in UA: {ua:?}"));
    rest.split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no numeric major in UA: {ua:?}"))
}

/// The stealth UA must claim the Chrome major the real binary actually is.
///
/// This locks the Chrome *version probe* against a real Chrome binary — which
/// nothing else in the suite did, and that gap is exactly how two bugs shipped
/// together:
///
/// 1. The probe exec'd `chrome.exe --version`, which on Windows starts a
///    browser that never exits, hanging `launch()` forever and *silently*
///    (a blocking call inside the future starves every tokio timeout).
/// 2. Where it did *not* hang — a dev box with Chrome already open — Windows
///    Chrome answered `--version` with "Opening in existing browser session.",
///    which carries no version token, so the probe silently fell back to a
///    baked-in major and shipped a UA claiming Chrome 148 on a real Chrome 150.
///
/// Bug 2 is the insidious one: a UA that contradicts the browser rendering it is
/// precisely the inconsistency stealth exists to avoid, and it failed *open*.
/// Asserting against the fallback constant would not catch it (the constant gets
/// bumped); asserting against the real browser does.
///
/// Non-circular by construction: the real major comes from a `stealth(off)`
/// browser reporting its own UA, the claimed major from a `native()` profile
/// whose UA is composed from the probe. Network-free — `about:blank` only.
#[tokio::test]
#[serial]
async fn stealth_ua_major_matches_the_real_chrome_binary() {
    let real_major = {
        let browser = Browser::builder()
            .headless(true)
            .stealth(StealthProfile::off())
            .launch()
            .await
            .expect("launch with stealth off");
        let ua: String = browser
            .main_tab()
            .evaluate("navigator.userAgent")
            .await
            .expect("read real UA");
        browser.close().await.expect("close");
        chrome_major_from_ua(&ua)
    };

    let claimed_major = {
        let browser = Browser::builder()
            .headless(true)
            .stealth(StealthProfile::native())
            .launch()
            .await
            .expect("launch with stealth native");
        let ua: String = browser
            .main_tab()
            .evaluate("navigator.userAgent")
            .await
            .expect("read stealth UA");
        browser.close().await.expect("close");
        chrome_major_from_ua(&ua)
    };

    assert_eq!(
        claimed_major, real_major,
        "stealth UA claims Chrome {claimed_major} but the real binary is Chrome \
         {real_major}; the version probe is falling back instead of detecting \
         the installed Chrome",
    );
}

/// cdpdriver/zendriver#33 + #198: long loops of launch/close in the Python
/// port accumulated zombie Chrome processes until the host OOMed. The Rust
/// port pairs `Browser::close` (graceful SIGTERM-then-SIGKILL) with a `Drop`
/// impl that relies on `kill_on_drop(true)` for panic safety. This test
/// asserts the combination reaps cleanly across many cycles:
/// `TestBrowser::close` checks that each launch's own process tree, zombies
/// included, is gone. It tracks only the Chrome it launched, so other
/// Chromes starting or exiting on the machine meanwhile can't fail it.
///
/// 25 cycles is enough to surface a single-process-per-iteration leak
/// without making CI brutal. Runs on every platform, Windows included — the
/// platform this bug class actually shipped on.
#[tokio::test]
#[serial]
async fn repeated_launch_close_leaves_no_orphan_chrome_processes() {
    for _ in 0..25 {
        let browser = launch_for_test(Browser::builder()).await;
        // Best-effort navigation; about:blank should never fail, and the
        // close below is what's under test.
        let _ = browser.main_tab().goto("about:blank").await;
        browser.close().await;
    }
}

/// cdpdriver/zendriver#89: `tab.send(...)` in the Python port could hang
/// indefinitely if the browser closed while a CDP call was in flight — the
/// pending future was never resolved. The Rust port uses a transport actor
/// that signals shutdown to every in-flight call, so each pending future
/// MUST resolve (Ok or Err) once `Browser::close` runs. This test races N
/// long-promise evaluations against an early close and asserts none of them
/// hang past a generous 5s timeout.
#[tokio::test]
#[serial]
async fn browser_close_during_inflight_cdp_calls_does_not_hang() {
    use std::sync::Arc;
    use tokio::time::timeout;

    const N: usize = 10;

    let browser = Browser::builder()
        .headless(true)
        .launch()
        .await
        .expect("launch");

    // Drive the main tab to a real (non about:blank) execution context so
    // the isolated-world resolver has a stable target.
    let main = browser.main_tab();
    main.goto("about:blank").await.expect("goto");

    // Shared so closures captured by tasks see the same handle without
    // requiring `'static` lifetimes against a stack-local clone vec.
    let tab = Arc::new(main);

    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let tab = Arc::clone(&tab);
        handles.push(tokio::spawn(async move {
            // 1s server-side delay. Some calls finish before close, others
            // race the close; both outcomes are valid — the contract is
            // "resolves, doesn't hang", not "succeeds".
            let result: Result<serde_json::Value, _> = tab
                .evaluate("new Promise(r => setTimeout(() => r(42), 1000))")
                .await;
            (i, result)
        }));
    }

    // Brief headstart so the calls are demonstrably in flight when close
    // lands, not racing the spawn loop.
    tokio::time::sleep(Duration::from_millis(50)).await;
    browser.close().await.expect("close");

    let mut hung = Vec::new();
    for h in handles {
        match timeout(Duration::from_secs(5), h).await {
            Ok(Ok((_i, _call_result))) => {
                // Either Ok(_) (raced and won) or Err(Transport/Cdp) (raced
                // and lost) is fine — both are "resolved, not hung".
            }
            Ok(Err(join_err)) => {
                panic!("task panicked while awaiting in-flight call: {join_err}");
            }
            Err(_elapsed) => {
                hung.push(());
            }
        }
    }

    assert!(
        hung.is_empty(),
        "{} of {} in-flight CDP calls hung past close (5s timeout)",
        hung.len(),
        N,
    );
}
