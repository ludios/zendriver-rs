//! `BrowserBuilder::improve_chrome_binary`, against real Chrome.
//!
//! A headless Chrome's own user agent says `HeadlessChrome`. zendriver's
//! override replaces it in pages, but a shared or service worker reads
//! Chrome's own, in `navigator.userAgent` and in its requests' `User-Agent`
//! header. The improved copy of Chrome has no `Headless` in its own.

#![cfg(all(feature = "integration-tests", target_os = "linux"))]

use serde_json::{Value, json};
use zendriver::Browser;
use zendriver_testkit::axum::Router;
use zendriver_testkit::axum::http::HeaderMap;
use zendriver_testkit::axum::http::header::{CONTENT_TYPE, USER_AGENT};
use zendriver_testkit::axum::response::Html;
use zendriver_testkit::axum::routing::get;
use zendriver_testkit::{FixtureServer, eval, launch_for_test, load};

/// JS that defines `report()`, a promise of what the current context reads
/// of the user agent: its own, the one that its requests send (as `/echo`
/// answers), and the high-entropy client hint `architecture`.
const REPORT: &str = "const report = async () => ({
  own: navigator.userAgent,
  sent: await (await fetch(location.origin + '/echo')).text(),
  architecture: (await navigator.userAgentData.getHighEntropyValues(['architecture'])).architecture,
});";

/// JS that evaluates to a promise of the [`REPORT`] of the page, of a shared
/// worker and of a service worker.
const REPORTS_JS: &str = "(async () => {
  const shared = new SharedWorker('/shared-worker.js').port;
  shared.start();
  const fromShared = new Promise(resolve => shared.onmessage = e => resolve(e.data));
  await navigator.serviceWorker.register('/service-worker.js');
  const registration = await navigator.serviceWorker.ready;
  const fromService = new Promise(resolve => navigator.serviceWorker.onmessage = e => resolve(e.data));
  registration.active.postMessage('report');
  return {page: await report(), sharedWorker: await fromShared, serviceWorker: await fromService};
})()";

/// The fixture's routes: `/page` with [`REPORT`], `/echo`, which answers the
/// request's `User-Agent`, and the two workers' scripts.
fn routes() -> Router {
    let script = |body: &str| {
        let source = format!("{REPORT}\n{body}");
        get(move || async move { ([(CONTENT_TYPE, "text/javascript")], source) })
    };
    let echo = |headers: HeaderMap| async move {
        headers[USER_AGENT]
            .to_str()
            .expect("an ASCII User-Agent")
            .to_owned()
    };
    Router::new()
        .route("/page", get(|| async { Html(format!("<!doctype html><script>{REPORT}</script>")) }))
        .route("/echo", get(echo))
        .route(
            "/shared-worker.js",
            script("onconnect = e => report().then(r => e.ports[0].postMessage(r))"),
        )
        .route(
            "/service-worker.js",
            script(
                "addEventListener('install', () => skipWaiting());
                 addEventListener('activate', e => e.waitUntil(clients.claim()));
                 addEventListener('message', e => e.waitUntil(report().then(r => e.source.postMessage(r))))",
            ),
        )
}

#[tokio::test]
async fn workers_of_an_improved_headless_chrome_send_no_headless() {
    let server = FixtureServer::start_with(routes()).await;
    let browser = launch_for_test(Browser::builder().improve_chrome_binary(true)).await;
    let tab = browser.main_tab();
    // 127.0.0.1 is a secure context, which service workers need.
    load(&tab, &server.url("127.0.0.1", "/page")).await;
    let reports = eval(&tab, REPORTS_JS).await;
    browser.close().await;

    let page = &reports["page"];
    let user_agent = page["own"].as_str().expect("a user agent");
    assert!(!user_agent.contains("Headless"), "{user_agent}");
    assert_ne!(page["architecture"], json!(""));
    let same_as_page =
        json!({"own": user_agent, "sent": user_agent, "architecture": page["architecture"]});
    let workers: Vec<&Value> = vec![&reports["sharedWorker"], &reports["serviceWorker"]];
    assert_eq!(workers, vec![&same_as_page, &same_as_page], "{reports:#}");
}
