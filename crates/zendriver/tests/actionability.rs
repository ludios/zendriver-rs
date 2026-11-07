//! What the actionability checks decide, against real Chrome.

#![cfg(feature = "integration-tests")]

use zendriver::Browser;
use zendriver_testkit::axum::Router;
use zendriver_testkit::axum::response::Html;
use zendriver_testkit::axum::routing::get;
use zendriver_testkit::{FixtureServer, launch_for_test, load};

/// Controls disabled in each way that HTML and ARIA allow, and controls
/// that only look like they might be.
const DISABLED_PAGE: &str = r#"<!doctype html>
<button id="enabled">Enabled</button>
<button id="own" disabled>Its own attribute</button>
<button id="aria" aria-disabled="true">ARIA</button>
<fieldset disabled>
  <legend><button id="in-legend">In the first legend</button></legend>
  <button id="in-fieldset">In a disabled fieldset</button>
</fieldset>
<select>
  <optgroup label="Group" disabled><option id="in-optgroup">In a disabled optgroup</option></optgroup>
  <option id="option">Outside it</option>
</select>
<div id="div">Not a control</div>"#;

#[tokio::test]
async fn is_enabled_follows_every_way_to_disable_a_control() {
    let page = Router::new().route("/disabled", get(|| async { Html(DISABLED_PAGE) }));
    let server = FixtureServer::start_with(page).await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/disabled")).await;

    let expected = [
        ("enabled", true),
        ("own", false),
        ("aria", false),
        ("in-legend", true),
        ("in-fieldset", false),
        ("in-optgroup", false),
        ("option", true),
        ("div", true),
    ];
    for (id, enabled) in expected {
        let el = tab.find().css(format!("#{id}")).one().await.expect(id);
        let actual = el.is_enabled().await.expect("is_enabled");
        assert_eq!(actual, enabled, "is_enabled() of #{id}");
    }
    browser.close().await;
}
