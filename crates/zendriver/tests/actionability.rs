//! What the actionability checks decide, against real Chrome. So far: when
//! an element is enabled.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use std::time::Duration;

use zendriver::Browser;
use zendriver_testkit::axum::Router;
use zendriver_testkit::axum::response::Html;
use zendriver_testkit::axum::routing::get;
use zendriver_testkit::{FixtureServer, eval, launch_for_test, load};

/// Elements that are disabled through their own attribute (a custom
/// element's included), through an ancestor (a `<fieldset>`, an
/// `<optgroup>`, a `<button>`, an `aria-disabled` container), or through
/// `aria-disabled` in upper case; and enabled elements next to them, among
/// them a control in a disabled fieldset's first `<legend>`, a control
/// whose `aria-disabled="false"` overrides its container's `true`, an
/// element that isn't a control, and a form whose control named
/// `closest` hides the form's own `closest`.
const DISABLED_PAGE: &str = r#"<!doctype html>
<button id="enabled">Enabled</button>
<button id="own" disabled>Its own attribute</button>
<button id="aria" aria-disabled="TRUE">ARIA in upper case</button>
<fieldset id="fieldset" disabled>
  <legend><button id="in-legend">In the first legend</button></legend>
  <button id="in-fieldset">In a disabled fieldset</button>
</fieldset>
<select>
  <optgroup label="Group" disabled><option id="in-optgroup">In a disabled optgroup</option></optgroup>
  <option id="option">Outside it</option>
</select>
<button disabled><span id="in-button">In a disabled button</span></button>
<div aria-disabled="true">
  <button id="in-aria-group">In an aria-disabled group</button>
  <button id="aria-false" aria-disabled="false">Overrides its group</button>
</div>
<my-switch id="custom" disabled>A custom element</my-switch>
<my-switch id="custom-enabled">A custom element</my-switch>
<form id="form"><input name="closest"></form>
<div id="div">Not a control</div>
<script>
customElements.define('my-switch', class extends HTMLElement {
  get disabled() { return this.hasAttribute('disabled'); }
});
</script>"#;

/// The page at `/disabled` on a new fixture server.
async fn server() -> FixtureServer {
    let page = Router::new().route("/disabled", get(|| async { Html(DISABLED_PAGE) }));
    FixtureServer::start_with(page).await
}

#[tokio::test]
async fn is_enabled_follows_the_disabled_states_of_html_and_aria() {
    let server = server().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/disabled")).await;

    let expected = [
        ("enabled", true),
        ("own", false),
        ("aria", false),
        ("fieldset", false),
        ("in-legend", true),
        ("in-fieldset", false),
        ("in-optgroup", false),
        ("option", true),
        ("in-button", false),
        ("in-aria-group", false),
        ("aria-false", true),
        ("custom", false),
        ("custom-enabled", true),
        ("form", true),
        ("div", true),
    ];
    for (id, enabled) in expected {
        let el = tab.find().css(format!("#{id}")).one().await.expect(id);
        let actual = el
            .is_enabled()
            .await
            .unwrap_or_else(|e| panic!("is_enabled() of #{id}: {e}"));
        assert_eq!(actual, enabled, "is_enabled() of #{id}");
    }
    browser.close().await;
}

#[tokio::test]
async fn a_click_waits_for_its_fieldset_to_be_enabled() {
    let server = server().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/disabled")).await;
    eval(
        &tab,
        "document.getElementById('in-fieldset').onclick = () => { window.clicked = true; }; true",
    )
    .await;

    let button = tab.find().css("#in-fieldset").one().await.expect("button");
    let click = tokio::spawn(async move { button.click().await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!click.is_finished(), "the click went ahead while disabled");
    eval(
        &tab,
        "document.getElementById('fieldset').disabled = false; true",
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), click)
        .await
        .expect("click within 10 s")
        .expect("click task")
        .expect("click");
    assert_eq!(eval(&tab, "window.clicked === true").await, true);
    browser.close().await;
}

/// A button in a closed shadow root, one that an overlay covers, and one
/// under an overlay that pointer events pass through.
const POINTER_PAGE: &str = r#"<!doctype html>
<div id="host"></div>
<div style="position: relative">
  <button id="covered" onclick="window.coveredClicked = true">Covered</button>
  <div style="position: absolute; inset: 0; background: rgba(0, 0, 0, 0.5)"></div>
</div>
<div style="position: relative">
  <button id="see-through" onclick="window.seeThroughClicked = true">See-through</button>
  <div style="position: absolute; inset: 0; pointer-events: none"></div>
</div>
<script>
document.getElementById('host').attachShadow({ mode: 'closed' }).innerHTML =
  '<button onclick="window.shadowClicked = true">In a closed shadow root</button>';
</script>"#;

/// The hit test sees into a closed shadow root and through
/// `pointer-events: none`, and refuses a click that an overlay covers.
#[tokio::test]
async fn a_click_lands_where_chromes_hit_test_says() {
    let page = Router::new().route("/pointer", get(|| async { Html(POINTER_PAGE) }));
    let server = FixtureServer::start_with(page).await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/pointer")).await;

    let shadow = tab
        .find()
        .role_named(zendriver::AriaRole::Button, "closed shadow root")
        .one()
        .await
        .expect("the button in the closed shadow root");
    shadow.click().await.expect("click in the shadow root");
    assert_eq!(eval(&tab, "window.shadowClicked === true").await, true);

    let see_through = tab.find().css("#see-through").one().await.expect("button");
    see_through
        .click()
        .await
        .expect("click through the overlay");
    assert_eq!(eval(&tab, "window.seeThroughClicked === true").await, true);

    let covered = tab.find().css("#covered").one().await.expect("button");
    match tokio::time::timeout(Duration::from_secs(15), covered.click()).await {
        Ok(Err(zendriver::ZendriverError::NotActionable(_, reason))) => {
            assert!(reason.contains("occluded"), "{reason}");
        }
        other => panic!("expected NotActionable for #covered, got {other:?}"),
    }
    assert_eq!(eval(&tab, "window.coveredClicked === true").await, false);
    browser.close().await;
}
