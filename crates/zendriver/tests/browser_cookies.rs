//! Looking up and deleting cookies through the tab-level and browser-level
//! jars, against real Chrome. Partly ported from the `cookies.rs` repro.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use serde_json::json;
use url::Url;
use zendriver::{Browser, Cookie, CookieJar, Tab};
use zendriver_testkit::{FixtureServer, SITE_B, TestBrowser, eval, launch_for_test, load};

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
async fn browser_cookies_find_by_url_and_delete() {
    let server = FixtureServer::start().await;
    let (browser, tab, url) = browser_with_cookie(&server).await;
    find_and_delete(browser.cookies(), tab.cookies(), &url, "browser").await;
    browser.close().await;
}

/// The cookies [`for_url_agrees_with_chrome`] has pages set: the page's
/// host, and what it assigns to `document.cookie`.
const VARIED_COOKIES: &[(&str, &str)] = &[
    ("a.test", "host_only=1"),
    ("a.test", "dotted=1; domain=a.test"),
    ("a.test", "deep=1; path=/plain"),
    ("a.test", "deep_slash=1; path=/plain/"),
    ("sub.a.test", "sub_only=1"),
    ("127.0.0.1", "loopback_secure=1; secure"),
];

/// The names of `cookies`, sorted.
fn names(cookies: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut names: Vec<String> = cookies.into_iter().collect();
    names.sort();
    names
}

/// `for_url` filters every cookie by RFC 6265's rules itself; Chrome's own
/// answer (`Network.getCookies` on the tab's session) must agree for every
/// scheme, host and path tried.
#[tokio::test]
async fn for_url_agrees_with_chrome() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    for (host, cookie) in VARIED_COOKIES {
        load(&tab, &server.url(host, "/plain")).await;
        eval(&tab, &format!("document.cookie = '{cookie}'")).await;
    }
    let all = browser.cookies().all().await.expect("all");
    let set = VARIED_COOKIES
        .iter()
        .map(|(_, c)| c.split('=').next().unwrap_or_default().to_string());
    assert_eq!(
        names(all.into_iter().map(|c| c.name)),
        names(set),
        "the cookies set"
    );

    let mut disagreements = Vec::new();
    for scheme in ["http", "https"] {
        for host in ["a.test", "sub.a.test", "x.sub.a.test", SITE_B, "127.0.0.1"] {
            for path in ["/", "/plain", "/plain/x", "/plainer"] {
                let url = format!("{scheme}://{host}:{}{path}", server.port());
                let ours = browser
                    .cookies()
                    .for_url(&Url::parse(&url).expect("URL"))
                    .await
                    .expect("for_url");
                let ours = names(ours.into_iter().map(|c| c.name));
                let chromes = tab
                    .session()
                    .call("Network.getCookies", json!({ "urls": [url] }))
                    .await
                    .expect("Network.getCookies");
                let chromes = names(
                    chromes["cookies"]
                        .as_array()
                        .expect("a cookies array")
                        .iter()
                        .map(|c| c["name"].as_str().expect("a name").to_string()),
                );
                if ours != chromes {
                    disagreements.push(format!("{url}: ours {ours:?}, Chrome's {chromes:?}"));
                }
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "for_url disagrees with Chrome:\n  {}",
        disagreements.join("\n  ")
    );
    browser.close().await;
}

/// With no domain or path, `delete` removes the name everywhere, including
/// a host-only cookie and a dotted one on the same host.
#[tokio::test]
async fn delete_by_name_alone_deletes_it_on_every_domain() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    for (host, cookie) in [
        ("a.test", "dup=1"),
        ("a.test", "dup=2; domain=a.test"),
        (SITE_B, "dup=3; path=/plain"),
        (SITE_B, "kept=4"),
    ] {
        load(&tab, &server.url(host, "/plain")).await;
        eval(&tab, &format!("document.cookie = '{cookie}'")).await;
    }
    let jar = browser.cookies();
    let all = jar.all().await.expect("all");
    assert_eq!(
        names(all.into_iter().map(|c| c.name)),
        ["dup", "dup", "dup", "kept"]
    );
    jar.delete("dup", None, None).await.expect("delete");
    let left = jar.all().await.expect("all");
    assert_eq!(names(left.into_iter().map(|c| c.name)), ["kept"]);
    browser.close().await;
}

/// A tab in an isolated browser context: its jar reads, finds and deletes
/// that context's cookies, and the browser's jar doesn't see them.
#[tokio::test]
#[ignore = "known bug, TODO P0 'Tab cookie jars use the default context': Storage.*Cookies on \
            the tab's session reads the default context, so all() (and, since 0b3e9bee5, \
            for_url() and delete()) miss an isolated context's cookies"]
async fn known_bug_tab_cookies_stay_in_an_isolated_context() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let context = browser
        .browser_context()
        .build()
        .await
        .expect("browser context");
    let tab = context.new_tab().await.expect("new_tab");
    let url = server.url("127.0.0.1", "/plain");
    load(&tab, &url).await;
    eval(&tab, &format!("document.cookie = '{NAME}=oat'")).await;
    let url = Url::parse(&url).expect("fixture URL");

    let all = tab.cookies().all().await.expect("tab all");
    assert!(has_cookie(&all), "tab.cookies().all(): {all:?}");
    let found = tab.cookies().for_url(&url).await.expect("tab for_url");
    assert!(has_cookie(&found), "tab.cookies().for_url(): {found:?}");
    let default = browser.cookies().all().await.expect("browser all");
    assert!(
        !has_cookie(&default),
        "browser.cookies().all(): {default:?}"
    );
    tab.cookies()
        .delete(NAME, None, None)
        .await
        .expect("tab delete");
    let left = tab.cookies().all().await.expect("tab all");
    assert!(!has_cookie(&left), "tab.cookies().delete left: {left:?}");
    browser.close().await;
}
