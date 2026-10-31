//! What counts as visible, against real Chrome. For `visible_only`:
//! rendered, and somewhere the page can scroll to, in the viewport or not.
//! For `is_visible`: rendered, and in the viewport now.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use zendriver::{Browser, Element};
use zendriver_testkit::{FixtureServer, launch_for_test, load};

async fn id(el: &Element) -> String {
    el.attr("id")
        .await
        .expect("read id")
        .expect("an element with an id")
}

#[tokio::test]
async fn visible_only_keeps_below_the_fold_but_not_off_the_page() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/below-fold")).await;

    let visible = tab
        .find_all()
        .css("button")
        .visible_only(true)
        .many()
        .await
        .expect("find visible buttons");
    let mut ids = Vec::new();
    for el in &visible {
        ids.push(id(el).await);
    }
    assert_eq!(ids, ["below"], "visible_only(true)");

    // `is_visible` is about the viewport, now.
    let below = tab.find().css("#below").one().await.expect("#below");
    assert!(
        !below.is_visible().await.expect("is_visible"),
        "#below before scrolling"
    );
    below.scroll_into_view().await.expect("scroll_into_view");
    assert!(
        below.is_visible().await.expect("is_visible"),
        "#below after scrolling"
    );
    browser.close().await;
}
