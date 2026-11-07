//! Real-Chrome tests of where zendriver's own JavaScript runs: in its
//! isolated world, in the current document of the right frame.
//!
//! Gated behind the `integration-tests` feature so CI can skip on
//! Chrome-less runners; CI exercises these on the integration job.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use serde_json::{Value, json};
use zendriver::{Browser, Frame, Tab};
use zendriver_testkit::axum::Router;
use zendriver_testkit::axum::response::Html;
use zendriver_testkit::axum::routing::get;
use zendriver_testkit::{FixtureServer, SITE_A, SITE_B, assert_eventually, launch_for_test, load};

/// How many same-origin iframes `/many-frames` embeds. The page's renderer
/// process numbers its execution contexts from 1 as it creates them, and
/// each iframe adds at least one, so the contexts of these iframes take
/// every small id.
const IFRAMES: usize = 12;

/// A fixture server that also serves `/many-frames`: a page titled
/// `Many frames`, with an `#heading`, and [`IFRAMES`] iframes of the canned
/// `/frame-child`.
async fn server() -> FixtureServer {
    let iframes = r#"<iframe src="/frame-child"></iframe>"#.repeat(IFRAMES);
    let page = format!(
        r#"<!doctype html><title>Many frames</title><h1 id="heading">Many frames</h1>{iframes}"#
    );
    let routes = Router::new().route("/many-frames", get(|| async { Html(page) }));
    FixtureServer::start_with(routes).await
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

/// `tab`'s only iframe, once the frame registry has it.
async fn only_child_frame(tab: &Tab) -> Frame {
    assert_eventually(Duration::from_secs(10), "the iframe registers", || async {
        tab.frames().await.unwrap().iter().any(|f| !f.is_main())
    })
    .await;
    let frames = tab.frames().await.unwrap();
    frames.into_iter().find(|f| !f.is_main()).unwrap()
}

/// After a cross-site navigation, `Tab::evaluate`, `Frame::evaluate` and
/// `Element::evaluate` must run in the new document of the main frame.
///
/// A cross-site navigation moves the page into a new renderer process, and
/// the new process numbers its execution contexts from 1 again. So the id of
/// a context of the previous page can name a context of the new page: here,
/// one of an iframe's. A call that keeps using such an id gets no error; it
/// reads the wrong document.
#[tokio::test]
async fn evaluate_reads_the_new_page_after_a_cross_site_navigation() {
    let server = server().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    let frame = tab.main_frame().await.unwrap();

    load(&tab, &server.url(SITE_A, "/plain")).await;
    let heading = tab.find().css("#heading").one().await.unwrap();
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

    browser.close().await;
}

/// A `Frame` handle must keep finding elements after its iframe navigates.
#[tokio::test]
async fn frame_find_reads_the_new_document_after_the_frame_navigates() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();

    load(&tab, &server.url(SITE_A, "/iframe-same-origin")).await;
    let child = only_child_frame(&tab).await;
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
