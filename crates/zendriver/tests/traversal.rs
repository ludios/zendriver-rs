//! `Element::children` against real Chrome.

#![cfg(feature = "integration-tests")]

use zendriver::Browser;
use zendriver_testkit::{FixtureServer, SITE_A, launch_for_test, load};

/// An `Element` can wrap a text node, which an XPath `text()` step returns.
/// A text node has no `children` collection, and `children()` returns an
/// empty `Vec` for it instead of failing.
#[tokio::test]
async fn children_of_a_text_node_is_empty() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;

    let text = tab
        .find()
        .xpath("//h1/text()")
        .one()
        .await
        .expect("/plain's h1 has a text node");
    let children = text
        .children()
        .await
        .expect("children() of a text node must not fail");
    assert!(children.is_empty(), "a text node has no children");

    drop(text);
    browser.close().await;
}
