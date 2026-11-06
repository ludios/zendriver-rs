//! `browser_intercept_*` rules driving a real Chrome through the server,
//! with the browser's own tracker blocking on the same tab.
//!
//! The server launches Chrome itself, without the `*.test` host mapping, so
//! these load the fixture pages from `127.0.0.1`, and use `localhost` (the
//! same fixture server) as the "tracker".

#![cfg(all(
    feature = "integration-tests",
    feature = "interception",
    feature = "tracker-blocking",
    target_os = "linux"
))]
#![allow(clippy::panic)]

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use support::{McpServer, Transport};
use zendriver_testkit::{FixtureServer, assert_eventually};

/// Bound on waiting for a page to load.
const LOAD_WITHIN: Duration = Duration::from_secs(10);

/// Load `url` in the current tab, waiting for its `load` event by polling
/// the page. A poll that fails, as one can while the page swaps documents,
/// is tried again.
async fn load(server: &McpServer, url: &str) {
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
}

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
/// with a 200 whose body is `body`, inserted at `position`.
fn respond(pattern: &str, body: &str, position: Value) -> Value {
    json!({
        "pattern": pattern,
        "action": { "kind": "respond", "status": 200, "body": body, "content_type": "text/plain" },
        "position": position,
    })
}

/// The `(origin, label or pattern or hosts, action kind)` of each entry of
/// a `rules` list, in order.
fn entries(rules: &Value) -> Vec<(String, String, String)> {
    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let Some(rules) = rules.as_array() else {
        panic!("not a list of rules: {rules}");
    };
    rules
        .iter()
        .map(|r| {
            let what = [&r["label"], &r["pattern"]]
                .into_iter()
                .find(|v| v.is_string())
                .map_or_else(|| r["hosts"].to_string(), text);
            (text(&r["origin"]), what, text(&r["action"]["kind"]))
        })
        .collect()
}

/// `(origin, what, kind)` as [`entries`] gives it.
fn entry(origin: &str, what: &str, kind: &str) -> (String, String, String) {
    (origin.into(), what.into(), kind.into())
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
    load(&server, &fixtures.url("127.0.0.1", "/plain")).await;

    let a = server
        .call(
            "browser_intercept_add_rule",
            respond("*/a.txt", "A", json!("last")),
        )
        .await;
    server
        .call(
            "browser_intercept_add_rule",
            respond("*/b.txt", "B", json!("last")),
        )
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

/// A `modify_response` rule changes the response it matches.
#[tokio::test]
async fn a_modify_response_rule_changes_the_status() {
    let fixtures = FixtureServer::start().await;
    let mut server = McpServer::start(Transport::Stdio).await;
    server.open(json!({})).await;
    load(&server, &fixtures.url("127.0.0.1", "/plain")).await;

    let rule = json!({
        "pattern": "*/plain?modified",
        "action": { "kind": "modify_response", "status": 203 },
        "position": "last",
    });
    server.call("browser_intercept_add_rule", rule).await;
    let status = server
        .call(
            "browser_evaluate",
            json!({ "expression": "fetch('/plain?modified').then(r => r.status)" }),
        )
        .await;
    assert_eq!(status["value"], 203);
}

/// A rule goes where its `position` says in the tab's list, which starts
/// with the browser's `browser.trackers`; the first entry that fits decides.
/// `move_rule` and `remove_rule` work on any entry, and `clear_rules`
/// removes only the agent's.
#[tokio::test]
async fn rules_take_their_place_in_the_list_with_the_tracker_entry() {
    let fixtures = FixtureServer::start().await;
    let tracker = format!("localhost:{}", fixtures.port());
    let mut server = McpServer::start(Transport::Stdio).await;
    server
        .open(json!({ "tracker_blocklist": { "source": "domains", "domains": ["localhost"] } }))
        .await;
    load(&server, &fixtures.url("127.0.0.1", "/plain")).await;

    let listed = server
        .call(
            "browser_intercept_list_rules",
            json!({ "include_hosts": true }),
        )
        .await;
    let trackers = &listed["rules"][0];
    assert_eq!(trackers["hosts"], json!(["localhost"]), "{listed}");
    assert_eq!(trackers["action"]["reason"], "BlockedByClient", "{listed}");
    let trackers = trackers["rule_id"].clone();

    let a = server
        .call(
            "browser_intercept_add_rule",
            respond("*/a.txt", "A", json!("last")),
        )
        .await;
    let ahead = server
        .call(
            "browser_intercept_add_rule",
            respond("*/a.txt", "ahead", json!({ "before": a["rule_id"] })),
        )
        .await;
    assert_eq!(
        entries(&ahead["rules"]),
        [
            entry("browser", "browser.trackers", "block"),
            entry("agent", "*/a.txt", "respond"),
            entry("agent", "*/a.txt", "respond"),
        ]
    );
    assert_eq!(ahead["rules"][1]["rule_id"], ahead["rule_id"]);
    assert_eq!(
        fetched(&server, &tracker).await,
        "200 ahead|404 |tracker blocked"
    );

    let allow = json!({
        "hosts": ["localhost"],
        "label": "allow the tracker",
        "action": { "kind": "continue" },
        "position": "first",
    });
    server.call("browser_intercept_add_rule", allow).await;
    assert_eq!(
        fetched(&server, &tracker).await,
        "200 ahead|404 |tracker loaded"
    );

    let moved = server
        .call(
            "browser_intercept_move_rule",
            json!({ "rule_id": trackers, "position": "first" }),
        )
        .await;
    assert_eq!(entries(&moved["rules"])[0].1, "browser.trackers");
    assert_eq!(
        fetched(&server, &tracker).await,
        "200 ahead|404 |tracker blocked"
    );

    let cleared = server
        .call("browser_intercept_clear_rules", json!({}))
        .await;
    assert_eq!(
        entries(&cleared["rules"]),
        [entry("browser", "browser.trackers", "block")]
    );
    let removed = server
        .call(
            "browser_intercept_remove_rule",
            json!({ "rule_id": trackers }),
        )
        .await;
    assert_eq!(removed["rules"], json!([]));
    assert_eq!(fetched(&server, &tracker).await, "404 |404 |tracker loaded");
}
