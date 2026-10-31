//! The fixtures, checked against a real Chrome: each canned page does what
//! the [`FixtureServer`] docs say, the two test sites really are two sites,
//! and the process-tree check can see a Chrome that's still running.
//!
//! These read Chrome's own state (`performance` timings, `fetch`, the DOM)
//! rather than going through zendriver's waits, which are what the fixtures
//! exist to test.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use zendriver::Browser;
use zendriver::stealth::StealthProfile;
use zendriver_testkit::{
    BASIC_AUTH_PASSWORD, BASIC_AUTH_USER, ChromeTree, DOWNLOAD_BODY, FixtureServer, SITE_A, SITE_B,
    TestBrowser, assert_eventually, assert_not_before, eval, launch_for_test, load, trickle_body,
    wait_loaded,
};

/// Bound on any one wait for the page; the slowest fixture used here takes
/// under a second.
const WAIT: Duration = Duration::from_secs(10);

async fn launch() -> TestBrowser {
    launch_for_test(Browser::builder()).await
}

#[tokio::test]
async fn test_sites_resolve_to_the_server_by_name() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();

    for site in [SITE_A, SITE_B] {
        let url = server.url(site, "/plain?title=Hello");
        load(&tab, &url).await;
        assert_eq!(eval(&tab, "document.title").await, json!("Hello"));
        assert_eq!(
            eval(&tab, "document.getElementById('host').textContent").await,
            json!(site)
        );
        assert_eq!(
            server.requests_to("/plain").last().map(|r| r.host.clone()),
            Some(site.into())
        );
    }

    browser.close().await;
}

#[tokio::test]
async fn slow_load_fires_load_after_the_slow_resource() {
    const DELAY_MS: u64 = 800;
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();

    load(
        &tab,
        &server.url(SITE_A, &format!("/slow-load?delay_ms={DELAY_MS}&iframe=1")),
    )
    .await;
    let loaded = Instant::now();

    let served = server
        .last_responded_at("/slow-resource")
        .expect("slow resource served");
    assert_not_before(loaded, served, "the slow resource was served");
    let load_start = eval(
        &tab,
        "performance.getEntriesByType('navigation')[0].loadEventStart",
    )
    .await;
    assert!(
        load_start.as_f64().expect("a number") >= DELAY_MS as f64,
        "load fired {load_start} ms into the navigation, before the {DELAY_MS} ms resource",
    );
    // The iframe is the part that's meant to finish first.
    let child_loaded = server.requests_to("/frame-child")[0]
        .responded_at
        .expect("child served");
    assert!(
        child_loaded < served,
        "the iframe should be served before the slow resource"
    );

    browser.close().await;
}

#[tokio::test]
async fn form_submit_navigates_to_the_result() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();

    load(&tab, &server.url(SITE_A, "/form")).await;
    eval(
        &tab,
        "document.getElementById('q').value = 'a b'; \
         document.getElementById('form').requestSubmit(); 0",
    )
    .await;
    wait_loaded(&tab, &server.url(SITE_A, "/form-result?q=a+b")).await;
    assert_eq!(
        eval(&tab, "document.getElementById('q').textContent").await,
        json!("a b")
    );

    browser.close().await;
}

#[tokio::test]
async fn redirect_chain_ends_at_plain() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();

    tab.goto(server.url(SITE_A, "/redirect/3"))
        .await
        .expect("goto");
    let end = server.url(SITE_A, "/plain?title=Redirected");
    assert_eventually(WAIT, "the redirect chain ends", || async {
        tab.evaluate_main::<String>("location.href")
            .await
            .is_ok_and(|href| href == end)
    })
    .await;
    let paths: Vec<String> = server
        .requests()
        .iter()
        .map(|r| r.uri.to_string())
        .collect();
    assert_eq!(
        paths,
        [
            "/redirect/3",
            "/redirect/2",
            "/redirect/1",
            "/plain?title=Redirected"
        ],
    );

    browser.close().await;
}

#[tokio::test]
async fn same_origin_iframe_is_scriptable_from_the_parent() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();

    load(&tab, &server.url(SITE_A, "/iframe-same-origin")).await;
    assert_eq!(
        eval(
            &tab,
            "document.getElementById('child').contentDocument \
             .getElementById('child-host').textContent"
        )
        .await,
        json!(SITE_A),
    );

    browser.close().await;
}

/// Under stock Chrome (site isolation on), a cross-site iframe gets its own
/// process, which shows up as an `iframe` target: that's Chrome agreeing
/// the two test hosts are different sites.
#[tokio::test]
async fn cross_site_iframe_is_out_of_process_under_site_isolation() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder().stealth(StealthProfile::off())).await;
    let tab = browser.main_tab();

    load(&tab, &server.url(SITE_A, "/iframe-cross-site")).await;
    assert_eq!(
        eval(&tab, "document.getElementById('child').contentDocument").await,
        Value::Null
    );
    let child = server.requests_to("/frame-child");
    assert_eq!(child.len(), 1);
    assert_eq!(child[0].host, SITE_B);

    let child_url = server.url(SITE_B, "/frame-child");
    let targets = browser
        .cdp()
        .call_raw("Target.getTargets", json!({}), None)
        .await
        .expect("Target.getTargets");
    let iframes: Vec<&Value> = targets["targetInfos"]
        .as_array()
        .expect("targetInfos")
        .iter()
        .filter(|t| t["type"] == "iframe")
        .collect();
    assert!(
        iframes.iter().any(|t| t["url"] == child_url.as_str()),
        "no out-of-process iframe target for {child_url}: {iframes:?}",
    );

    browser.close().await;
}

#[tokio::test]
async fn downloads_are_attachments() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();

    load(&tab, &server.url(SITE_A, "/downloads")).await;
    let fetched = eval(
        &tab,
        "Promise.all(['/download', '/download-tiny'].map(async u => { \
           const r = await fetch(u); \
           return [r.headers.get('content-disposition'), await r.text()]; \
         }))",
    )
    .await;
    assert_eq!(
        fetched,
        json!([
            ["attachment; filename=\"report.csv\"", DOWNLOAD_BODY],
            ["attachment; filename=\"tiny.txt\"", "x"],
        ]),
    );

    browser.close().await;
}

#[tokio::test]
async fn basic_auth_challenges_until_given_the_password() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();

    load(&tab, &server.url(SITE_A, "/plain")).await;
    // `credentials: 'omit'`, or the 401 raises an auth prompt nobody answers.
    let js = format!(
        "Promise.all([ \
           {{}}, \
           {{Authorization: 'Basic ' + btoa('{BASIC_AUTH_USER}:{BASIC_AUTH_PASSWORD}')}}, \
         ].map(async headers => {{ \
             const r = await fetch('/basic-auth', {{headers, credentials: 'omit'}}); \
             return [r.status, r.headers.get('www-authenticate')]; \
           }}))"
    );
    assert_eq!(
        eval(&tab, &js).await,
        json!([[401, "Basic realm=\"fixture\""], [200, null]])
    );

    browser.close().await;
}

#[tokio::test]
async fn trickle_sends_its_body_in_timed_chunks() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();

    load(&tab, &server.url(SITE_A, "/plain")).await;
    let timed = eval(
        &tab,
        "(async () => { \
           const t0 = performance.now(); \
           const r = await fetch('/trickle?chunks=4&chunk_bytes=10&interval_ms=200'); \
           const head = performance.now() - t0; \
           const body = await r.text(); \
           return [head, performance.now() - t0, body]; \
         })()",
    )
    .await;
    let (head_ms, total_ms) = (
        timed[0].as_f64().expect("ms"),
        timed[1].as_f64().expect("ms"),
    );
    assert!(
        head_ms < 400.0,
        "headers took {head_ms} ms; they should come with the first chunk"
    );
    assert!(
        total_ms >= 600.0,
        "body took {total_ms} ms; three 200 ms gaps should take 600"
    );
    let body = String::from_utf8(trickle_body(4, 10)).expect("ASCII");
    assert_eq!(timed[2], json!(body));

    browser.close().await;
}

/// Each static page renders the elements tests rely on.
#[tokio::test]
async fn static_pages_render() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();

    let checks = [
        (
            "/plain",
            "document.getElementById('link').getAttribute('href') === '/plain?title=Linked'",
        ),
        (
            "/controls",
            "document.getElementById('fruit').value === 'banana' \
             && document.getElementById('notes').value === 'Some notes' \
             && document.getElementById('editable').isContentEditable",
        ),
        ("/quirks", "document.compatMode === 'BackCompat'"),
        ("/plain", "document.compatMode === 'CSS1Compat'"),
        (
            "/svg-sprite",
            "['use-href', 'use-xlink', 'use-external'] \
             .every(id => document.querySelector(`#${id} use`).href instanceof SVGAnimatedString)",
        ),
        (
            "/below-fold",
            "document.getElementById('below').getBoundingClientRect().top > innerHeight",
        ),
        (
            "/padded-button",
            "(() => { \
               const b = document.getElementById('padded'); \
               return b.clientLeft === 25 && b.offsetWidth - b.clientWidth === 50 \
                 && getComputedStyle(b).paddingLeft === '60px' && Array.isArray(window.clicks); \
             })()",
        ),
    ];
    for (path, check) in checks {
        load(&tab, &server.url(SITE_A, path)).await;
        assert_eq!(eval(&tab, check).await, json!(true), "{path}: {check}");
    }
    assert!(
        !server.requests_to("/sprite.svg").is_empty(),
        "the external sprite wasn't fetched"
    );

    browser.close().await;
}

/// The trap the table fixture sets: the cell whose text is exactly the
/// link's is wider than the link, so its centre isn't on the link.
#[tokio::test]
async fn table_link_cell_centre_misses_the_link() {
    let server = FixtureServer::start().await;
    let browser = launch().await;
    let tab = browser.main_tab();

    load(&tab, &server.url(SITE_A, "/table-link")).await;
    let hit = eval(
        &tab,
        "(() => { \
           const td = document.getElementById('readme-link').parentElement; \
           const r = td.getBoundingClientRect(); \
           const hit = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2); \
           return [td.textContent, hit.tagName]; \
         })()",
    )
    .await;
    assert_eq!(hit, json!(["README.md", "TD"]));

    browser.close().await;
}

/// The exit check can see a running Chrome, zygotes included (they're only
/// found by their command line), so its silence after close means something.
#[tokio::test]
async fn process_tree_check_sees_a_running_chrome() {
    let browser = launch().await;
    let mut tree = ChromeTree::of(&browser).await;
    assert!(
        tree.snapshot(&browser).await > 1,
        "expected Chrome's helper processes too"
    );

    let alive = tree
        .wait_gone(Duration::from_millis(200))
        .await
        .expect_err("Chrome is still running, so its processes can't be gone");
    if cfg!(target_os = "linux") {
        assert!(
            alive.iter().any(|p| p.contains("--type=zygote")),
            "no zygote among {alive:#?}"
        );
    }

    browser.close().await;
}

/// The capture sees events from zendriver's own tasks.
#[tokio::test]
async fn logs_capture_zendriver_events() {
    let browser = launch().await;
    let launched = browser.logs().lines();
    assert!(
        launched
            .iter()
            .any(|l| l.target.starts_with("zendriver") && l.message == "launching chrome"),
        "the launch wasn't logged: {launched:#?}",
    );
    browser.close().await;
}

/// Closing an earlier browser first doesn't end a later one's capture.
#[tokio::test]
async fn closing_an_earlier_browser_keeps_a_later_ones_logs() {
    const MESSAGE: &str = "logged after the first browser closed";
    let first = launch().await;
    let second = launch().await;
    first.close().await;
    tracing::warn!("{MESSAGE}");
    let warnings = second.logs().at_least(tracing::Level::WARN);
    assert!(
        warnings.iter().any(|l| l.message == MESSAGE),
        "the later browser's capture missed it: {warnings:#?}",
    );
    second.close().await;
}
