//! Looking up and deleting cookies through the tab-level and browser-level
//! jars, against real Chrome. Ported from the `cookies.rs` repro.
//!
//! Tests named `known_bug_*` reproduce an open bug, named in their
//! `#[ignore]` reason; the fix deletes the `#[ignore]` and the prefix.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use url::Url;
use zendriver::{Browser, Cookie, CookieJar, Tab};
use zendriver_testkit::{FixtureServer, TestBrowser, eval, launch_for_test, load};

/// The cookie each test sets, on 127.0.0.1.
const NAME: &str = "flavour";

/// Launch a browser with `NAME` set by a page on 127.0.0.1; returns the
/// browser, its tab, and the page's URL.
async fn browser_with_cookie(server: &FixtureServer) -> (TestBrowser, Tab, Url) {
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    let url = server.url("127.0.0.1", "/plain");
    load(&tab, &url).await;
    eval(&tab, &format!("document.cookie = '{NAME}=oat'")).await;
    let url = Url::parse(&url).expect("fixture URL");
    (browser, tab, url)
}

fn has_cookie(cookies: &[Cookie]) -> bool {
    cookies.iter().any(|c| c.name == NAME)
}

/// Find `NAME` through `jar` by URL, delete it, and check (through
/// `check`, a jar known to work) that it's gone. `which` names `jar`.
async fn find_and_delete(jar: CookieJar, check: CookieJar, url: &Url, which: &str) {
    let found = jar
        .for_url(url)
        .await
        .unwrap_or_else(|e| panic!("{which}.cookies().for_url: {e}"));
    assert!(
        has_cookie(&found),
        "{which}.cookies().for_url missed {NAME}: {found:?}"
    );
    jar.delete(NAME, Some("127.0.0.1"), None)
        .await
        .unwrap_or_else(|e| panic!("{which}.cookies().delete: {e}"));
    let left = check.all().await.expect("tab.cookies().all");
    assert!(
        !has_cookie(&left),
        "{which}.cookies().delete left {NAME}: {left:?}"
    );
}

#[tokio::test]
async fn tab_cookies_find_by_url_and_delete() {
    let server = FixtureServer::start().await;
    let (browser, tab, url) = browser_with_cookie(&server).await;
    find_and_delete(tab.cookies(), tab.cookies(), &url, "tab").await;
    browser.close().await;
}

#[tokio::test]
#[ignore = "known bug, TODO P0 'Browser-level cookies().for_url() / .delete() fail with \
            \"method not found\"' (R5, MEC #3)"]
async fn known_bug_browser_cookies_find_by_url_and_delete() {
    let server = FixtureServer::start().await;
    let (browser, tab, url) = browser_with_cookie(&server).await;
    find_and_delete(browser.cookies(), tab.cookies(), &url, "browser").await;
    browser.close().await;
}
