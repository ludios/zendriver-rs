//! `browser_intercept_*` rules driving a real Chrome through the server,
//! with the browser's own tracker blocking on the same tab.
//!
//! The server launches Chrome itself, without the `*.test` host mapping, so
//! these load the fixture pages from `127.0.0.1`, and use `localhost` (the
//! same fixture server) as the "tracker".

#![cfg(all(feature = "integration-tests", target_os = "linux"))]
#![allow(clippy::panic)]

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use support::{McpServer, Transport};
use zendriver_testkit::{FixtureServer, assert_eventually};

/// Bound on waiting for a page to load.
const LOAD_WITHIN: Duration = Duration::from_secs(10);

/// What the page's `fetch` of `/a.txt`, of `/b.txt`, and of a page on the
/// tracker host `tracker` (a `host:port`) get, joined by `|`: a status and
/// body, or for the tracker, whether it was blocked.
async fn fetched(server: &McpServer, tracker: &str) -> Value {
    let expression = format!(
        "Promise.all([\
           fetch('/a.txt').then(async r => r.status + ' ' + await r.text()),\
           fetch('/b.txt').then(async r => r.status + ' ' + await r.text()),\
           fetch('http://{tracker}/plain', {{ mode: 'no-cors' }})\
             .then(() => 'tracker loaded', () => 'tracker blocked'),\
         ]).then(r => r.join('|'))"
    );
    server
        .call("browser_evaluate", json!({ "expression": expression }))
        .await["value"]
        .clone()
}

/// `rule` for `browser_intercept_add_rule`: answer requests for `pattern`
/// with a 200 whose body is `body`.
fn respond(pattern: &str, body: &str) -> Value {
    json!({
        "pattern": pattern,
        "action": { "kind": "respond", "status": 200, "body": body, "content_type": "text/plain" },
    })
}

/// Two rules and the browser's tracker blocking all work on one tab, and
/// removing one rule leaves the other rule and the tracker blocking
/// working.
#[tokio::test]
async fn removing_one_rule_leaves_the_others_and_tracker_blocking_working() {
    let fixtures = FixtureServer::start().await;
    let tracker = format!("localhost:{}", fixtures.port());
    let mut server = McpServer::start(Transport::Stdio).await;
    server
        .open(json!({ "tracker_blocklist": { "source": "domains", "domains": ["localhost"] } }))
        .await;
    let url = fixtures.url("127.0.0.1", "/plain");
    server.call("browser_goto", json!({ "url": url })).await;
    assert_eventually(LOAD_WITHIN, &format!("{url} finishes loading"), || async {
        let state = server
            .try_call(
                "browser_evaluate",
                json!({ "expression": "location.href + ' ' + document.readyState" }),
            )
            .await;
        state.is_ok_and(|s| s["value"] == format!("{url} complete"))
    })
    .await;

    let a = server
        .call("browser_intercept_add_rule", respond("*/a.txt", "A"))
        .await;
    server
        .call("browser_intercept_add_rule", respond("*/b.txt", "B"))
        .await;
    assert_eq!(
        fetched(&server, &tracker).await,
        "200 A|200 B|tracker blocked"
    );

    server
        .call(
            "browser_intercept_remove_rule",
            json!({ "rule_id": a["rule_id"] }),
        )
        .await;
    assert_eq!(
        fetched(&server, &tracker).await,
        "404 |200 B|tracker blocked"
    );
}
