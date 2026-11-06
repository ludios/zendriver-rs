//! Network interception (`Tab::intercept`) against real Chrome and the
//! fixture server.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use futures::StreamExt;
use zendriver::interception::{Match, Position};
use zendriver::{Browser, RequestOverrides, ResponseOverrides};
use zendriver_testkit::{
    FixtureServer, PROXY_PASSWORD, PROXY_USER, SITE_A, SITE_B, eval, launch_for_test, load,
};

/// What a `fetch` of `path` from the page gets:
/// `"<status> [<statusText>] <content-type>"`, or `"timeout"` after 5 s (a
/// request left paused never settles).
async fn fetch_response(tab: &zendriver::Tab, path: &str) -> String {
    let js = format!(
        "Promise.race([\
           fetch('{path}').then(r =>\
             `${{r.status}} [${{r.statusText}}] ${{r.headers.get('content-type')}}`),\
           new Promise(done => setTimeout(() => done('timeout'), 5000)),\
         ])"
    );
    eval(tab, &js).await.as_str().unwrap().to_string()
}

/// A response rule that changes only the status keeps the upstream headers
/// (Chrome refuses a `continueResponse` with a status but no headers), and a
/// rule that asks for an empty phrase gets one (Chrome refuses `""`).
#[tokio::test]
async fn response_rules_that_set_only_a_status_or_a_phrase() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;

    let _intercept = tab
        .intercept()
        .modify_response("*/plain?status", |_| ResponseOverrides {
            status: Some(203),
            ..ResponseOverrides::default()
        })
        .unwrap()
        .modify_response("*/plain?no-phrase", |_| ResponseOverrides {
            phrase: Some(String::new()),
            ..ResponseOverrides::default()
        })
        .unwrap()
        .start(Position::Last)
        .await
        .unwrap();

    assert_eq!(
        fetch_response(&tab, "/plain?status").await,
        "203 [Non-Authoritative Information] text/html; charset=utf-8"
    );
    assert_eq!(
        fetch_response(&tab, "/plain?no-phrase").await,
        "200 [] text/html; charset=utf-8"
    );
    browser.close().await;
}

/// `start` returns once Chrome intercepts: a request the page sends right
/// after it is already answered by the rule.
#[tokio::test]
async fn a_request_right_after_start_is_intercepted() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;

    let _mock = tab
        .intercept()
        .respond("*/plain?mocked", 200, Vec::new(), b"Mocked".to_vec())
        .unwrap()
        .start(Position::Last)
        .await
        .unwrap();
    let body = eval(&tab, "fetch('/plain?mocked').then(r => r.text())").await;
    assert_eq!(body, "Mocked");
    browser.close().await;
}

/// A rule of the caller's and the browser's proxy credentials work on the
/// same tab, and stopping the rule leaves the credentials working.
#[tokio::test]
async fn a_rule_and_the_proxy_credentials_compose() {
    let server = FixtureServer::start().await;
    let builder = Browser::builder()
        .proxy(server.proxy_url())
        .proxy_auth(PROXY_USER, PROXY_PASSWORD);
    let browser = launch_for_test(builder).await;
    let tab = browser.main_tab();

    let page = b"<title>Mocked</title>".to_vec();
    let html = vec![("content-type".to_string(), "text/html".to_string())];
    let mock = tab
        .intercept()
        .respond("*/plain?mocked", 200, html, page)
        .unwrap()
        .start(Position::Last)
        .await
        .unwrap();
    load(&tab, &server.url(SITE_A, "/plain?mocked")).await;
    assert_eq!(eval(&tab, "document.title").await, "Mocked");
    load(&tab, &server.url(SITE_A, "/plain")).await;
    assert_eq!(eval(&tab, "document.title").await, "Plain");

    mock.stop().await.unwrap();
    load(&tab, &server.url(SITE_A, "/plain?mocked")).await;
    assert_eq!(eval(&tab, "document.title").await, "Plain");
    browser.close().await;
}

/// A response rule needs no pattern of its own: the layer pauses at the
/// Response stage for `modify_response` (and at the Request stage for the
/// others), so `modify_response` alone changes the response, and a block
/// rule after it still blocks.
#[tokio::test]
async fn rules_pause_at_their_own_stage() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;

    let _rules = tab
        .intercept()
        .modify_response("*/plain?*", |_| ResponseOverrides {
            status: Some(203),
            ..ResponseOverrides::default()
        })
        .unwrap()
        .block("*/plain?blocked")
        .unwrap()
        .start(Position::Last)
        .await
        .unwrap();
    assert_eq!(
        fetch_response(&tab, "/plain?modified").await,
        "203 [Non-Authoritative Information] text/html; charset=utf-8"
    );
    let blocked = "fetch('/plain?blocked').then(() => 'loaded', () => 'blocked')";
    assert_eq!(eval(&tab, blocked).await, "blocked");
    browser.close().await;
}

/// A paused request is released whatever goes wrong with it: a rule's
/// closure that panics, a response that Chrome refuses (a header name with
/// a space), or a stream's request dropped unreleased. Each request still
/// gets the server's response.
#[tokio::test]
async fn a_paused_request_is_released_whatever_goes_wrong() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    let upstream = "200 [OK] text/html; charset=utf-8";

    let bad_header = vec![("bad header".to_string(), "x".to_string())];
    let _rules = tab
        .intercept()
        .modify_request("*/plain?panic", |_| -> RequestOverrides {
            panic!("a bug in the closure")
        })
        .unwrap()
        .respond("*/plain?refused", 200, bad_header, Vec::new())
        .unwrap()
        .start(Position::Last)
        .await
        .unwrap();
    assert_eq!(fetch_response(&tab, "/plain?panic").await, upstream);
    assert_eq!(fetch_response(&tab, "/plain?refused").await, upstream);

    let held = Match::url("*/plain?held").unwrap();
    let mut stream = tab
        .interceptor()
        .subscribe(held, Position::Last)
        .await
        .unwrap();
    let held = tokio::spawn({
        let tab = tab.clone();
        async move { fetch_response(&tab, "/plain?held").await }
    });
    let paused = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the stream got no request")
        .unwrap();
    drop(paused);
    assert_eq!(held.await.unwrap(), upstream);
    browser.close().await;
}

/// With tracker blocking, a tab starts with the entry `browser.trackers`,
/// and a catch-all rule inserted at `Last` comes after it: tracker requests
/// stay blocked, the others get the rule. Moved to `First`, the rule
/// decides the tracker requests too, and they go through. (The page can't
/// tell: Chrome hides a cross-site `no-cors` HTML response from it, so the
/// server's log says what went through.)
#[tokio::test]
async fn tracker_blocking_decides_ahead_of_a_rule_behind_it() {
    let server = FixtureServer::start().await;
    let builder = Browser::builder().tracker_blocklist_add([SITE_B.to_string()]);
    let browser = launch_for_test(builder).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    let tracker = format!(
        "fetch('{}', {{ mode: 'no-cors' }}).then(() => 0, () => 0)",
        server.url(SITE_B, "/plain?tracked")
    );
    let tracked = || {
        let to_tracker = server.requests_to("/plain");
        to_tracker.iter().filter(|r| r.host == SITE_B).count()
    };

    let tag = |req: &zendriver::RequestInfo| {
        let mut headers = req.headers.clone();
        headers.push(("x-tag".into(), "1".into()));
        RequestOverrides {
            headers: Some(headers),
            ..RequestOverrides::default()
        }
    };
    let rule = tab
        .intercept()
        .modify_request("*", tag)
        .unwrap()
        .start(Position::Last)
        .await
        .unwrap();
    let trackers = tab.tracker_blocking().unwrap();
    let snapshot = tab.interceptor().snapshot(false).await.unwrap();
    let order: Vec<_> = snapshot.entries.iter().map(|e| e.id).collect();
    assert_eq!(order, [trackers, rule.ids()[0]]);
    assert_eq!(
        snapshot.entries[0].label.as_deref(),
        Some("browser.trackers")
    );

    eval(&tab, &tracker).await;
    assert_eq!(tracked(), 0, "the tracker request went through");
    eval(&tab, "fetch('/plain?tagged').then(r => r.status)").await;
    let tagged = server.requests_to("/plain");
    assert!(
        tagged.iter().any(|r| r.header("x-tag") == Some("1")),
        "the rule never tagged a request: {tagged:?}"
    );

    tab.interceptor()
        .move_rule(rule.ids()[0], Position::First)
        .await
        .unwrap();
    eval(&tab, &tracker).await;
    assert_eq!(tracked(), 1, "the tracker request didn't go through");
    browser.close().await;
}
