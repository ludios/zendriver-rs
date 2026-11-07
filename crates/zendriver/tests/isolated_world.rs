//! Real-Chrome tests of where zendriver's own JavaScript runs: in its
//! isolated world, in the current document of the right frame.
//!
//! Gated behind the `integration-tests` feature so CI can skip on
//! Chrome-less runners; CI exercises these on the integration job.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use serde_json::{Value, json};
use zendriver::{Browser, Frame, Tab, ZendriverError};
use zendriver_testkit::axum::Router;
use zendriver_testkit::axum::response::Html;
use zendriver_testkit::axum::routing::get;
use zendriver_testkit::{
    FixtureServer, SITE_A, SITE_B, assert_eventually, eval, launch_for_test, load,
};

/// How many same-origin iframes `/many-frames` embeds. A new renderer
/// process numbers its execution contexts from 1 as it creates them, and
/// each iframe adds at least one, so one of these iframes' contexts is
/// likely to get the id that the previous page's isolated world had. The
/// test that uses this checks that one did.
const IFRAMES: usize = 12;

/// A page script that wraps the DOM methods and getters that element
/// queries and actions use, so that each call of one from this page's main
/// world appends the method's name to `window.calls`. Calls from an
/// isolated world use that world's own, unwrapped, prototypes.
const HOOKS_JS: &str = r#"<script>
  window.calls = [];
  const methods = [
    [Document.prototype, ['querySelector', 'querySelectorAll', 'evaluate', 'elementFromPoint',
                          'elementsFromPoint']],
    [Element.prototype, ['querySelector', 'querySelectorAll', 'getBoundingClientRect',
                         'getClientRects', 'scrollIntoView', 'scrollIntoViewIfNeeded', 'matches',
                         'closest', 'getAttribute', 'checkVisibility']],
    [HTMLElement.prototype, ['focus', 'blur', 'click']],
    [EventTarget.prototype, ['dispatchEvent']],
    [window, ['getComputedStyle']],
  ];
  for (const [target, names] of methods) {
    for (const name of names) {
      const original = target[name];
      target[name] = function (...args) {
        window.calls.push(name);
        return original.apply(this, args);
      };
    }
  }
  const getters = [
    [Node.prototype, ['textContent', 'parentElement']],
    [HTMLElement.prototype, ['innerText']],
    [Element.prototype, ['innerHTML', 'outerHTML', 'children']],
    [HTMLInputElement.prototype, ['value']],
  ];
  for (const [proto, names] of getters) {
    for (const name of names) {
      const d = Object.getOwnPropertyDescriptor(proto, name);
      Object.defineProperty(proto, name, {
        ...d,
        get() {
          window.calls.push(name);
          return d.get.call(this);
        },
      });
    }
  }
</script>"#;

/// A fixture server that also serves:
///
/// - `/many-frames`: a page titled `Many frames`, with an `#heading`, and
///   [`IFRAMES`] iframes of the canned `/frame-child`.
/// - `/hooked`: a page with [`HOOKS_JS`], an `<input id="field">` that the
///   page gives the expando `pageProp = "page"`, a `#button` that sets
///   `window.clicked`, and an iframe of `/hooked-child`.
/// - `/hooked-child`: a page with [`HOOKS_JS`], a `#child-button` that sets
///   `window.clicked`, and the global `childGlobal = "child"`.
/// - `/two-frames`: iframes `#a` and `#b` of the canned `/plain`, titled `A`
///   and `B`.
async fn server() -> FixtureServer {
    let iframes = r#"<iframe src="/frame-child"></iframe>"#.repeat(IFRAMES);
    let many_frames = format!(
        r#"<!doctype html><title>Many frames</title><h1 id="heading">Many frames</h1>{iframes}"#
    );
    let hooked = format!(
        r#"<!doctype html><title>Hooked</title>{HOOKS_JS}
<input id="field">
<button id="button" onclick="window.clicked = true">Press me</button>
<iframe src="/hooked-child"></iframe>
<script>document.getElementById('field').pageProp = 'page';</script>"#
    );
    let hooked_child = format!(
        r#"<!doctype html><title>Hooked child</title>{HOOKS_JS}
<button id="child-button" onclick="window.clicked = true">Child button</button>
<script>window.childGlobal = 'child';</script>"#
    );
    let two_frames = r#"<!doctype html><title>Two frames</title>
<iframe id="a" src="/plain?title=A"></iframe><iframe id="b" src="/plain?title=B"></iframe>"#
        .to_string();
    let routes = Router::new()
        .route("/many-frames", get(|| async { Html(many_frames) }))
        .route("/two-frames", get(|| async { Html(two_frames) }))
        .route("/hooked", get(|| async { Html(hooked) }))
        .route("/hooked-child", get(|| async { Html(hooked_child) }));
    FixtureServer::start_with(routes).await
}

/// Take what [`HOOKS_JS`] recorded in `tab`'s main frame, leaving it empty.
async fn take_calls(tab: &Tab) -> Vec<String> {
    tab.evaluate_main("window.calls.splice(0)").await.unwrap()
}

/// The id of the execution context of zendriver's isolated world in the
/// current document of `tab`'s main frame. Chrome looks an isolated world up
/// by its name, so this is the context that zendriver's own calls use, and
/// not a new one. The name `zendriver-eval` is zendriver's.
async fn isolated_world_id(tab: &Tab) -> i64 {
    let reply = tab
        .session()
        .call(
            "Page.createIsolatedWorld",
            json!({ "frameId": tab.target_id(), "worldName": "zendriver-eval" }),
        )
        .await
        .unwrap();
    reply["executionContextId"].as_i64().unwrap()
}

/// `tab`'s iframe named `child`, once the frame registry has it under that
/// name. The registry replaces a frame's entry when it learns the frame's
/// name, and only the new entry's URL is kept current.
async fn child_frame(tab: &Tab) -> Frame {
    assert_eventually(Duration::from_secs(10), "the iframe registers", || async {
        tab.frame_by_name("child").await.unwrap().is_some()
    })
    .await;
    tab.frame_by_name("child").await.unwrap().unwrap()
}

/// After a cross-site navigation, `Tab::evaluate`, `Frame::evaluate` and
/// `Element::evaluate` must run in the new document of the main frame.
///
/// Under site isolation, a cross-site navigation moves the page into a new
/// renderer process, which numbers its execution contexts from 1 again. So
/// the id of a context of the previous page can name a context of the new
/// page: here, one of an iframe's. A call that keeps using such an id gets
/// no error; it reads the wrong document.
#[tokio::test]
async fn evaluate_reads_the_new_page_after_a_cross_site_navigation() {
    let server = server().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    let frame = tab.main_frame().await.unwrap();

    load(&tab, &server.url(SITE_A, "/plain")).await;
    let old_heading = tab.find().css("#heading").one().await.unwrap();
    let heading = old_heading.clone();
    let tab_title: String = tab.evaluate("document.title").await.unwrap();
    let frame_title: String = frame.evaluate("document.title").await.unwrap();
    let element_title: String = heading.evaluate("document.title").await.unwrap();
    assert_eq!([tab_title, frame_title, element_title], ["Plain"; 3]);
    let old_id = isolated_world_id(&tab).await;

    load(&tab, &server.url(SITE_B, "/many-frames")).await;
    // The trap must be live, or this test guards nothing: the old id must
    // now name a context of one of the iframes.
    let old_id_reads: Value = tab
        .session()
        .call(
            "Runtime.evaluate",
            json!({ "contextId": old_id, "expression": "location.pathname", "returnByValue": true }),
        )
        .await
        .unwrap_or_else(|e| panic!("context {old_id} must exist in the new page: {e}"));
    assert_eq!(
        old_id_reads["result"]["value"], "/frame-child",
        "the old id {old_id} must name a context of an iframe of the new page"
    );

    let heading = tab.find().css("#heading").one().await.unwrap();
    let tab_title: String = tab.evaluate("document.title").await.unwrap();
    let frame_title: String = frame.evaluate("document.title").await.unwrap();
    let element_title: String = heading.evaluate("document.title").await.unwrap();
    assert_eq!(tab_title, "Many frames", "Tab::evaluate");
    assert_eq!(frame_title, "Many frames", "Frame::evaluate");
    assert_eq!(element_title, "Many frames", "Element::evaluate");
    // An element kept from the previous page re-runs its query.
    let text: String = old_heading.evaluate("el.textContent").await.unwrap();
    assert_eq!(text, "Many frames", "Element::evaluate on a kept element");
    let text: String = old_heading.evaluate_main("el.textContent").await.unwrap();
    assert_eq!(
        text, "Many frames",
        "Element::evaluate_main on a kept element"
    );

    browser.close().await;
}

/// A `Frame` handle must keep finding elements after its iframe navigates.
#[tokio::test]
async fn frame_find_reads_the_new_document_after_the_frame_navigates() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();

    load(&tab, &server.url(SITE_A, "/iframe-same-origin")).await;
    let child = child_frame(&tab).await;
    let button = child.find().css("#child-button").one().await.unwrap();
    assert_eq!(button.inner_text().await.unwrap(), "Child button");

    let _: Value = tab
        .evaluate_main("document.getElementById('child').src = '/plain?title=Second'")
        .await
        .unwrap();
    assert_eventually(Duration::from_secs(10), "the iframe navigates", || async {
        child.url().await.contains("/plain")
    })
    .await;

    let heading = child.find().css("#heading").one().await.unwrap();
    assert_eq!(heading.inner_text().await.unwrap(), "Second");

    browser.close().await;
}

/// `Frame::evaluate_main` and `Frame::content` on an iframe in the page's
/// process run in the iframe's main world, not in the top-level page's, and
/// `evaluate_main` runs a whole script there.
#[tokio::test]
async fn evaluate_main_runs_in_the_main_world_of_a_same_process_iframe() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/iframe-same-origin")).await;
    let frame = child_frame(&tab).await;
    // A global of the iframe's main world, which no isolated world sees.
    eval(
        &tab,
        "document.getElementById('child').contentWindow.marker = 'the iframe'",
    )
    .await;

    let read: String = frame
        .evaluate_main("const suffix = '!'; window.marker + suffix")
        .await
        .unwrap();
    assert_eq!(read, "the iframe!");
    let html = frame.content().await.unwrap();
    assert!(html.contains("child-button"), "{html}");
    browser.close().await;
}

/// Element queries and actions on a tab must not run in the page's main
/// world, where page scripts can watch them (and falsify their answers)
/// through wrapped DOM methods and getters, while still having the effects
/// that the page sees.
#[tokio::test]
async fn element_queries_and_actions_are_invisible_to_page_hooks() {
    let server = server().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/hooked")).await;

    // The hooks must be live, or this test guards nothing.
    let _: Value = tab
        .evaluate_main("document.querySelectorAll('input')")
        .await
        .unwrap();
    assert_eq!(take_calls(&tab).await, ["querySelectorAll"]);

    let field = tab.find().css("#field").one().await.unwrap();
    let button = tab.find().xpath("//button").one().await.unwrap();
    tab.find().text("Press me").one().await.unwrap();
    tab.find().tag("button").one().await.unwrap();
    let body = button.parent().await.unwrap().unwrap();
    body.find().css("input").one().await.unwrap();
    assert!(field.is_visible().await.unwrap());
    assert!(field.bounding_box().await.unwrap().is_some());
    assert_eq!(field.attr("id").await.unwrap().as_deref(), Some("field"));
    assert!(field.outer_html().await.unwrap().contains("field"));
    assert_eq!(button.inner_text().await.unwrap(), "Press me");
    button.click().await.unwrap();
    field.set_value("first").await.unwrap();
    field.clear().await.unwrap();
    field.type_text("hi").await.unwrap();
    let page_prop: Option<String> = field.evaluate("el.pageProp").await.unwrap();
    assert_eq!(
        page_prop, None,
        "Element::evaluate must not see page expandos"
    );
    let value: String = field.evaluate("el.value").await.unwrap();
    assert_eq!(value, "hi");

    assert_eq!(take_calls(&tab).await, Vec::<String>::new());
    let clicked: bool = tab.evaluate_main("window.clicked === true").await.unwrap();
    assert!(clicked, "the click must reach the page");

    let page_prop: Option<String> = field.evaluate_main("el.pageProp").await.unwrap();
    assert_eq!(page_prop.as_deref(), Some("page"), "Element::evaluate_main");

    browser.close().await;
}

/// An element in an iframe has its frame's worlds: `Element::evaluate`
/// sees the iframe's own `document`, and `Element::evaluate_main` runs in
/// the iframe's main world.
#[tokio::test]
async fn an_iframe_element_evaluates_in_its_own_frames_worlds() {
    let server = server().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/hooked")).await;

    let button = tab
        .find()
        .css("#child-button")
        .include_frames()
        .one()
        .await
        .unwrap();
    let own_document: bool = button
        .evaluate("el.ownerDocument === document")
        .await
        .unwrap();
    assert!(
        own_document,
        "Element::evaluate must see the iframe's document"
    );
    let isolated_global: Option<String> = button.evaluate("window.childGlobal").await.unwrap();
    assert_eq!(
        isolated_global, None,
        "Element::evaluate must not see page globals"
    );
    let main_global: Option<String> = button.evaluate_main("window.childGlobal").await.unwrap();
    assert_eq!(
        main_global.as_deref(),
        Some("child"),
        "Element::evaluate_main"
    );

    button.click().await.unwrap();
    let (clicked, calls): (bool, Vec<String>) = button
        .evaluate_main("[window.clicked === true, window.calls.splice(0)]")
        .await
        .unwrap();
    assert!(clicked, "the click must reach the iframe's page");
    assert_eq!(calls, Vec::<String>::new(), "page hooks in the iframe");

    browser.close().await;
}

/// A `Frame` handle of a removed iframe must fail, rather than run in the
/// sibling iframe that is left.
#[tokio::test]
async fn a_removed_frames_handle_does_not_reach_its_sibling() {
    let server = server().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/two-frames")).await;
    assert_eventually(Duration::from_secs(10), "iframe A registers", || async {
        tab.frame_by_url("title=A").await.unwrap().is_some()
    })
    .await;
    let frame_a = tab.frame_by_url("title=A").await.unwrap().unwrap();
    let title: String = frame_a.evaluate("document.title").await.unwrap();
    assert_eq!(title, "A");

    eval(&tab, "document.getElementById('a').remove()").await;
    assert_eventually(Duration::from_secs(10), "iframe A detaches", || async {
        tab.frame_by_url("title=A").await.unwrap().is_none()
    })
    .await;

    let evaluated = frame_a.evaluate::<String>("document.title").await;
    assert!(
        matches!(evaluated, Err(ZendriverError::FrameNotFound(_))),
        "Frame::evaluate: {evaluated:?}"
    );
    let found = frame_a.find().css("title").one().await;
    assert!(
        matches!(found, Err(ZendriverError::FrameNotFound(_))),
        "Frame::find: {found:?}"
    );

    browser.close().await;
}
