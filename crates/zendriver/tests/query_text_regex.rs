//! `text_regex_with_flags` against real Chrome.

#![cfg(feature = "integration-tests")]

use zendriver::Browser;
use zendriver_testkit::{FixtureServer, SITE_A, eval, launch_for_test, load};

/// A `g` flag makes a JavaScript `RegExp`'s `test` start at its
/// `lastIndex`, which a match moves forward. The query reuses one `RegExp`
/// for every element, so each element must still be tested from the start
/// of its own text: all three `<p>`s match, not every other one.
#[tokio::test]
async fn a_global_regex_matches_every_element() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    eval(
        &tab,
        "document.body.innerHTML = '<p>Buy</p><p>Buy</p><p>Buy</p>'",
    )
    .await;

    let found = tab
        .find_all()
        .text_regex_with_flags("^Buy$", "g")
        .many()
        .await
        .expect("the <p>s match");
    assert_eq!(found.len(), 3, "every <p> must match");

    drop(found);
    browser.close().await;
}
