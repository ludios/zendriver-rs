//! What counts as visible, against real Chrome. `visible_only` and
//! `is_visible`: rendered, wherever the element is. An action: also in the
//! viewport, after scrolling the element there.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use std::time::Duration;

use zendriver::{Browser, Element, ZendriverError};
use zendriver_testkit::{FixtureServer, eval, launch_for_test, load};

async fn id(el: &Element) -> String {
    el.attr("id")
        .await
        .expect("read id")
        .expect("an element with an id")
}

#[tokio::test]
async fn visible_only_and_is_visible_ignore_where_the_element_is() {
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
    assert_eq!(
        ids,
        ["off-page", "faint-slot", "below", "skipped"],
        "visible_only(true)"
    );

    let all = tab.find_all().css("button").many().await.expect("buttons");
    for el in &all {
        let id = id(el).await;
        let visible = el.is_visible().await.expect("is_visible");
        assert_eq!(visible, ids.contains(&id), "is_visible() of #{id}");
    }
    browser.close().await;
}

#[tokio::test]
async fn an_action_needs_the_element_in_the_viewport() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/below-fold")).await;

    // Scrolling brings both into the viewport; `#skipped`'s section renders
    // once it's there.
    for (id, flag) in [("#below", "belowClicked"), ("#skipped", "skippedClicked")] {
        let el = tab.find().css(id).one().await.expect(id);
        el.click()
            .await
            .unwrap_or_else(|e| panic!("click {id}: {e}"));
        assert_eq!(eval(&tab, &format!("window.{flag}")).await, true, "{id}");
    }

    // Scrolling can't reach `left: -9999px`.
    let off_page = tab.find().css("#off-page").one().await.expect("#off-page");
    let res = tokio::time::timeout(Duration::from_secs(10), off_page.click()).await;
    match res {
        Ok(Err(ZendriverError::NotActionable(_, reason))) => {
            assert!(reason.contains("visible"), "{reason}");
        }
        other => panic!("expected NotActionable for #off-page, got {other:?}"),
    }
    browser.close().await;
}
