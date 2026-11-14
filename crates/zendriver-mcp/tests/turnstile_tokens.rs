//! `browser_solve_turnstile` keeping the tokens it returned, for its
//! `ignore_returned_tokens`, driving a real Chrome through the server.

#![cfg(all(
    feature = "integration-tests",
    feature = "cloudflare",
    target_os = "linux"
))]
#![allow(clippy::panic)]

mod support;

use axum::Router;
use axum::response::Html;
use axum::routing::get;
use serde_json::{Value, json};
use support::{McpServer, Transport};
use zendriver_testkit::FixtureServer;

/// What a widget left behind on a page: its token input, still holding its
/// token, and nothing else of the widget.
const LEFT_BEHIND: &str = r#"<!doctype html>
<title>Sign up</title>
<form><input type="hidden" name="cf-turnstile-response" value="old-token"></form>"#;

/// A call returns the token in the page and records it. A call with
/// `ignore_returned_tokens` then ignores that token, takes the input that
/// holds it for no challenge, and returns `already_clear`; a call without
/// the flag returns the old token again.
#[tokio::test]
async fn a_returned_token_is_ignored_on_request() {
    let routes = Router::new().route("/left", get(|| async { Html(LEFT_BEHIND) }));
    let fixtures = FixtureServer::start_with(routes).await;
    let mut server = McpServer::start(Transport::Stdio).await;
    server.open(json!({})).await;
    let url = fixtures.url("127.0.0.1", "/left");
    server.call("browser_goto", json!({ "url": url })).await;
    let solve = |arguments: Value| server.call("browser_solve_turnstile", arguments);

    let first = solve(json!({})).await;
    assert_eq!(
        (&first["outcome"], &first["token"]),
        (&json!("solved"), &json!("old-token"))
    );
    let ignoring = solve(json!({ "ignore_returned_tokens": true })).await;
    assert_eq!(ignoring["outcome"], "already_clear", "{ignoring}");
    let again = solve(json!({})).await;
    assert_eq!(again["token"], "old-token", "{again}");
}
