//! Role queries against real Chrome: they match the roles and accessible
//! names that Chrome's accessibility tree computes.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use std::time::Duration;

use zendriver::{AriaRole, Browser, Element};
use zendriver_testkit::axum::Router;
use zendriver_testkit::axum::response::Html;
use zendriver_testkit::axum::routing::get;
use zendriver_testkit::{FixtureServer, launch_for_test, load};

/// Links of every kind: by their tag (`implicit`, `in-list`), by a `role`
/// attribute (`explicit`), and in a closed shadow root (`shadow`), in that
/// document order; and what isn't a link to the accessibility tree: an
/// `<a>` without `href`, one under `aria-hidden`, one not rendered, and one
/// in an iframe's document. Also a text box named by its `<label>`.
const ROLES_PAGE: &str = r#"<!doctype html>
<a id="implicit" href="/a">Implicit</a>
<div id="explicit" role="link" tabindex="0">Explicit</div>
<a id="no-href">No href</a>
<a id="aria-hidden" href="/b" aria-hidden="true">Hidden from the tree</a>
<a id="display-none" href="/c" style="display: none">Not rendered</a>
<ul id="list"><li><a id="in-list" href="/d">In a list</a></li></ul>
<div id="host"></div>
<label>Email address <input id="email"></label>
<iframe srcdoc='<a id="framed" href="/e">Framed</a>'></iframe>
<script>
document.getElementById('host').attachShadow({ mode: 'closed' }).innerHTML =
  '<a id="shadow" href="/f">In a closed shadow root</a>';
</script>"#;

/// The `id` of each of `elements`.
async fn ids(elements: &[Element]) -> Vec<String> {
    let mut ids = Vec::new();
    for el in elements {
        let id = el.attr("id").await.expect("read id");
        ids.push(id.unwrap_or_default());
    }
    ids
}

#[tokio::test]
async fn role_matches_the_roles_and_names_that_chrome_computes() {
    let page = Router::new().route("/roles", get(|| async { Html(ROLES_PAGE) }));
    let server = FixtureServer::start_with(page).await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/roles")).await;

    let links = tab
        .find_all()
        .role(AriaRole::Link)
        .many()
        .await
        .expect("links");
    assert_eq!(
        ids(&links).await,
        ["implicit", "explicit", "in-list", "shadow"]
    );

    let list = tab.find().css("#list").one().await.expect("#list");
    let in_list = list
        .find_all()
        .role(AriaRole::Link)
        .many()
        .await
        .expect("links");
    assert_eq!(ids(&in_list).await, ["in-list"]);
    let lists = list
        .find_all()
        .role(AriaRole::List)
        .timeout(Duration::from_millis(500))
        .many_or_empty()
        .await
        .expect("lists");
    assert!(lists.is_empty(), "the scope's own element matched");

    let email = tab
        .find()
        .role_named(AriaRole::Textbox, "EMAIL")
        .one()
        .await
        .expect("the text box named by its label");
    assert_eq!(ids(&[email]).await, ["email"]);

    let named = tab
        .find_all()
        .role_named(AriaRole::Link, "shadow")
        .many()
        .await;
    assert_eq!(ids(&named.expect("links")).await, ["shadow"]);
    browser.close().await;
}

/// Under the default profile, focus emulation lets a background tab
/// render, so Chrome's accessibility tree answers a role query there too.
#[tokio::test]
async fn role_works_in_a_background_tab_under_the_default_profile() {
    let page = Router::new().route("/roles", get(|| async { Html(ROLES_PAGE) }));
    let server = FixtureServer::start_with(page).await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/roles")).await;
    let _front = browser.new_tab().await.expect("a tab in front");

    let link = tab.find().role(AriaRole::Link).one().await.expect("link");
    assert_eq!(ids(&[link]).await, ["implicit"]);
    browser.close().await;
}
