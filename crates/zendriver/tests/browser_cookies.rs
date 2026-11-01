//! Looking up and deleting cookies through the tab-level and browser-level
//! jars, against real Chrome. Partly ported from the `cookies.rs` repro.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use serde_json::json;
use url::Url;
use zendriver::{Browser, Cookie, CookieJar, CookiePartitionKey, Tab};
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
    ("localhost", "localhost_secure=1; secure"),
    ("localhost.", "dotted_localhost_secure=1; secure"),
];

/// The names of `cookies`, sorted.
fn names(cookies: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut names: Vec<String> = cookies.into_iter().collect();
    names.sort();
    names
}

/// The names of the cookies in `Network.getCookies`'s reply `reply`.
fn reply_names(reply: &serde_json::Value) -> Vec<String> {
    names(
        reply["cookies"]
            .as_array()
            .expect("a cookies array")
            .iter()
            .map(|c| c["name"].as_str().expect("a name").to_string()),
    )
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

    let hosts = [
        "a.test",
        "sub.a.test",
        "x.sub.a.test",
        SITE_B,
        "127.0.0.1",
        "localhost",
        "localhost.",
    ];
    let mut disagreements = Vec::new();
    for scheme in ["http", "https", "ws", "wss", "ftp", "file"] {
        for host in hosts {
            for path in ["/", "/plain", "/plain/x", "/plainer"] {
                let url = if scheme == "file" {
                    // A file URL has no port.
                    format!("file://{host}{path}")
                } else {
                    format!("{scheme}://{host}:{}{path}", server.port())
                };
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
                let chromes = reply_names(&chromes);
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

/// Of the partitioned cookies, `for_url` returns those that a page at the
/// URL sends: the ones partitioned under the URL's own site, set with no
/// cross-site ancestor. A subdomain is on its parent's site, except on
/// that of a dotless host such as `localhost`, which is a site of its own.
/// `Network.getCookies` answers for the page the tab shows, so the tab
/// shows the URL.
#[tokio::test]
async fn for_url_returns_only_the_urls_own_partition() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    let jar = browser.cookies();
    // (page host, cookie domain, [(name, partition)], the names it sends)
    let cases = [
        (
            "127.0.0.1",
            "127.0.0.1",
            vec![
                ("unpartitioned", None),
                ("own", Some(("http://127.0.0.1", false))),
                ("own_embedded", Some(("http://127.0.0.1", true))),
                ("other_site", Some(("http://a.test", true))),
            ],
            ["own", "unpartitioned"],
        ),
        // Secure cookies go to http pages only on loopback hosts.
        (
            "sub.a.localhost",
            ".a.localhost",
            vec![
                ("parent_site", Some(("http://a.localhost", false))),
                ("https_site", Some(("https://a.localhost", false))),
                ("bare_localhost", Some(("http://localhost", false))),
            ],
            ["parent_site", "unpartitioned"],
        ),
        (
            "a.localhost",
            "a.localhost",
            vec![
                ("own_localhost", Some(("http://a.localhost", false))),
                ("bare_localhost", Some(("http://localhost", false))),
            ],
            ["own_localhost", "unpartitioned"],
        ),
    ];
    for (host, domain, partitions, sent) in cases {
        jar.clear().await.expect("clear");
        let unpartitioned = [("unpartitioned", None)];
        for (name, partition) in partitions.iter().chain(&unpartitioned) {
            let partition_key = partition.map(|(site, cross)| CookiePartitionKey {
                top_level_site: site.into(),
                has_cross_site_ancestor: cross,
            });
            let cookie = Cookie {
                name: (*name).into(),
                value: "1".into(),
                domain: domain.into(),
                path: "/".into(),
                secure: true,
                partition_key,
                ..Default::default()
            };
            jar.set(cookie).await.expect("set");
        }

        let url = server.url(host, "/plain");
        load(&tab, &url).await;
        let ours = jar.for_url(&Url::parse(&url).expect("URL")).await;
        let ours = names(ours.expect("for_url").into_iter().map(|c| c.name));
        let chromes = tab
            .session()
            .call("Network.getCookies", json!({ "urls": [url] }))
            .await
            .expect("Network.getCookies");
        assert_eq!(reply_names(&chromes), sent, "Chrome's, at {host}");
        assert_eq!(ours, sent, "ours, at {host}");
    }
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

/// `delete` removes cookies that Chrome can't simply re-set as they are: a
/// `Secure` cookie that a loopback page set over http (its source scheme is
/// `NonSecure`), next to a `Secure` and a non-`Secure` one on another site.
#[tokio::test]
async fn delete_removes_secure_cookies_from_loopback_pages() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    for (host, cookie) in [
        ("a.test", "dup=1"),
        ("127.0.0.1", "dup=2; secure"),
        ("localhost", "dup=3; secure"),
    ] {
        load(&tab, &server.url(host, "/plain")).await;
        eval(&tab, &format!("document.cookie = '{cookie}'")).await;
    }
    let jar = browser.cookies();
    let secure = Cookie {
        name: "dup".into(),
        value: "4".into(),
        domain: ".a.test".into(),
        path: "/".into(),
        secure: true,
        ..Default::default()
    };
    jar.set(secure).await.expect("set");
    assert_eq!(jar.all().await.expect("all").len(), 4);
    jar.delete("dup", None, None).await.expect("delete");
    let left = jar.all().await.expect("all");
    assert!(left.is_empty(), "left: {left:?}");
    browser.close().await;
}

/// A tab in an isolated browser context: its jar reads, finds and deletes
/// that context's cookies, and the browser's jar doesn't see them.
#[tokio::test]
async fn tab_cookies_stay_in_an_isolated_context() {
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

/// `connect` to a Chrome whose only page is in an isolated context: the
/// main tab's jar reads that context, not the default one.
#[tokio::test]
async fn a_connected_main_tab_reads_its_isolated_context() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let context = browser
        .browser_context()
        .build()
        .await
        .expect("browser context");
    let tab = context.new_tab().await.expect("new_tab");
    load(&tab, &server.url("127.0.0.1", "/plain")).await;
    eval(&tab, &format!("document.cookie = '{NAME}=oat'")).await;
    let inspector = tab.inspector_url().expect("inspector url");
    let host_port = inspector
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_else(|| panic!("unexpected inspector url {inspector}"));
    browser
        .main_tab()
        .close()
        .await
        .expect("close the main tab");

    let connected = Browser::connect(format!("http://{host_port}"))
        .await
        .expect("connect");
    let all = connected.main_tab().cookies().all().await.expect("all");
    assert!(has_cookie(&all), "the main tab's jar: {all:?}");
    let default = connected.cookies().all().await.expect("browser all");
    assert!(!has_cookie(&default), "the default jar: {default:?}");
    connected.close().await.expect("close the connection");
    browser.close().await;
}

/// A default-context tab's jar names no context: Chrome before ~153 knows
/// only the contexts DevTools created, and rejects the default one's id.
/// (Chrome 156 accepts it, so this reads the jar's `Debug`.)
#[tokio::test]
async fn only_isolated_tab_jars_name_their_context() {
    let browser = launch_for_test(Browser::builder()).await;
    let new_tab = browser.new_tab().await.expect("new_tab");
    for tab in [browser.main_tab(), new_tab] {
        let jar = format!("{:?}", tab.cookies());
        assert!(jar.contains("browser_context_id: None"), "{jar}");
    }
    let context = browser
        .browser_context()
        .build()
        .await
        .expect("browser context");
    let isolated = context.new_tab().await.expect("isolated tab");
    let jar = format!("{:?}", isolated.cookies());
    assert!(
        jar.contains(&format!("browser_context_id: Some({:?})", context.id())),
        "{jar}"
    );
    browser.close().await;
}
