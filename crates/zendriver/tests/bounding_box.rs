//! `Element::bounding_box` against real Chrome.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use zendriver::Browser;
use zendriver_testkit::{FixtureServer, eval, launch_for_test, load};

/// The box is the border box, the one `getBoundingClientRect()` reports,
/// on a button whose padding and border are large and differ per axis, so
/// that another of its boxes, or a mix of two, would differ by many pixels.
#[tokio::test]
async fn bounding_box_is_the_border_box() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/padded-button")).await;

    let button = tab.find().css("#padded").one().await.expect("#padded");
    let bbox = button
        .bounding_box()
        .await
        .expect("bounding_box")
        .expect("a box");
    let rect = eval(
        &tab,
        "(() => { const r = document.getElementById('padded').getBoundingClientRect(); \
         return [r.x, r.y, r.width, r.height]; })()",
    )
    .await;
    let rect: Vec<f64> = serde_json::from_value(rect).expect("four numbers");
    let ours = [bbox.x, bbox.y, bbox.width, bbox.height];
    for (axis, (ours, chromes)) in ["x", "y", "width", "height"]
        .iter()
        .zip(ours.iter().zip(&rect))
    {
        assert!(
            (ours - chromes).abs() < 0.5,
            "{axis}: bounding_box {ours}, getBoundingClientRect {chromes}"
        );
    }
    browser.close().await;
}
