//! The targets that Chrome reports only to a page's own DevTools session:
//! its cross-site iframes, its dedicated workers and its worklets.
//!
//! A cross-site iframe runs in another renderer process (it is an
//! out-of-process iframe), where no override or script of the page's session
//! reaches: zendriver has to attach it and set it up like the page, so that
//! the identity it reads is its page's. Without that, a headless launch's
//! cross-site iframes report `HeadlessChrome`, and the host's locale and
//! timezone. Those tests serve the page from `127.0.0.1` and its iframes from
//! `localhost`: two sites, and both secure contexts, which
//! `navigator.userAgentData` and `navigator.deviceMemory` need.
//!
//! Attaching those iframes makes Chrome hold back the page's dedicated
//! workers and worklets until zendriver attaches and releases them too.

#![cfg(feature = "integration-tests")]

use std::time::Duration;

use serde_json::{Value, json};
use tracing::Level;
use zendriver::stealth::StealthProfile;
use zendriver::{Browser, BrowserBuilder, Persona};
use zendriver_testkit::axum::Router;
use zendriver_testkit::axum::extract::Host;
use zendriver_testkit::axum::http::header::CONTENT_TYPE;
use zendriver_testkit::axum::response::Html;
use zendriver_testkit::axum::routing::get;
use zendriver_testkit::{
    FixtureServer, PINNED_HARDWARE, assert_eventually, eval, launch_for_test, load,
};

/// JS that evaluates to a promise of what a document reads of the browser's
/// identity, including the user agent that a dedicated worker of the
/// document reads (so the promise settles only if that worker runs).
const IDENTITY_JS: &str = r#"(async () => {
  const worker = new Worker(URL.createObjectURL(new Blob(['postMessage(navigator.userAgent)'])));
  const uaData = navigator.userAgentData;
  const hints = await uaData.getHighEntropyValues(
    ['architecture', 'fullVersionList', 'platformVersion', 'uaFullVersion']);
  return {
    userAgent: navigator.userAgent,
    workerUserAgent: await new Promise(resolve => worker.onmessage = e => resolve(e.data)),
    platform: navigator.platform,
    brands: uaData.brands,
    hints,
    languages: navigator.languages,
    locale: Intl.DateTimeFormat().resolvedOptions().locale,
    timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
    hardwareConcurrency: navigator.hardwareConcurrency,
    deviceMemory: navigator.deviceMemory,
    webdriver: navigator.webdriver,
  };
})()"#;

/// A fixture server with `/identity-page`, which has two iframes of
/// `/identity-frame` on `localhost`, and collects in `window.frameIdentities`
/// the [`IDENTITY_JS`] that each one posts to it.
async fn server() -> FixtureServer {
    let frame = format!(
        "<!doctype html><script>{IDENTITY_JS}.then(identity => parent.postMessage(identity, '*'))</script>"
    );
    let page = |Host(host): Host| async move {
        let frame_url = format!(
            "http://{}/identity-frame",
            host.replace("127.0.0.1", "localhost")
        );
        Html(format!(
            "<!doctype html>\
             <script>window.frameIdentities = [];\
             onmessage = e => window.frameIdentities.push(e.data)</script>\
             <iframe src='{frame_url}'></iframe><iframe src='{frame_url}'></iframe>"
        ))
    };
    let routes = Router::new()
        .route("/identity-page", get(page))
        .route("/identity-frame", get(move || async move { Html(frame) }));
    FixtureServer::start_with(routes).await
}

/// Load `/identity-page` in a browser launched from `builder`, check that
/// both of its cross-site iframes read the identity that the page reads, and
/// return that identity.
async fn identity_shared_with_cross_site_iframes(builder: BrowserBuilder) -> Value {
    let server = server().await;
    let browser = launch_for_test(builder).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/identity-page")).await;
    let posted = || async { eval(&tab, "window.frameIdentities.length").await == json!(2) };
    assert_eventually(
        Duration::from_secs(10),
        "both iframes posted their identity",
        posted,
    )
    .await;
    let page = eval(&tab, IDENTITY_JS).await;
    let frames = eval(&tab, "window.frameIdentities").await;
    let iframe_targets = browser
        .cdp()
        .call_raw("Target.getTargets", json!({}), None)
        .await
        .expect("Target.getTargets")["targetInfos"]
        .as_array()
        .expect("targetInfos")
        .iter()
        .filter(|target| target["type"] == "iframe")
        .count();
    browser.logs().assert_none_at_least(Level::WARN);
    browser.close().await;
    assert_eq!(
        iframe_targets, 2,
        "the iframes must be out-of-process, or this test checks nothing"
    );
    assert_eq!(frames, json!([page, page]), "the iframes' identities");
    page
}

/// A persona that pins a locale and a timezone other than the host's.
fn german() -> Persona {
    r#"{"locale": "de-DE", "timezone": "Europe/Berlin"}"#
        .parse()
        .expect("persona JSON")
}

#[tokio::test]
async fn cross_site_iframes_read_the_pages_identity_under_native() {
    let identity =
        identity_shared_with_cross_site_iframes(Browser::builder().persona_overlay(german())).await;
    let user_agent = identity["userAgent"].as_str().expect("userAgent");
    assert!(!user_agent.contains("Headless"), "{user_agent}");
    assert_eq!(identity["workerUserAgent"], identity["userAgent"]);
    assert_eq!(identity["locale"], "de-DE");
    assert_eq!(identity["timeZone"], "Europe/Berlin");
}

/// Under `spoofed`, the bootstrap script reaches the iframes too: it is what
/// reports the persona's `deviceMemory`.
#[tokio::test]
async fn cross_site_iframes_read_the_pages_identity_under_spoofed() {
    let pinned: Persona = PINNED_HARDWARE.parse().expect("persona JSON");
    let builder = Browser::builder()
        .stealth(StealthProfile::spoofed())
        .persona_overlay(german().overlay(pinned));
    let identity = identity_shared_with_cross_site_iframes(builder).await;
    assert_eq!(identity["hardwareConcurrency"], 1);
    assert_eq!(identity["deviceMemory"], 1);
    assert_eq!(identity["timeZone"], "Europe/Berlin");
}

/// JS that evaluates to a promise of `{kind: "ran"}` for each kind of worker
/// and worklet that a page can start, or `{kind: why}` for one that didn't
/// run within 5 seconds. `/shared-worker.js` and `/service-worker.js` are
/// [`worker_routes`]'.
const START_WORKERS_JS: &str = r#"(async () => {
  const blob = source => URL.createObjectURL(new Blob([source], {type: 'text/javascript'}));
  const answer = worker => new Promise(resolve => worker.onmessage = e => resolve(e.data));
  const processor = 'class extends AudioWorkletProcessor { process() { return true } }';
  const starts = {
    dedicatedWorker: () => answer(new Worker(blob('postMessage("ran")'))),
    nestedWorker: () => answer(new Worker(blob(
      `new Worker(URL.createObjectURL(new Blob(['postMessage("ran")']))).onmessage =
         e => postMessage(e.data)`))),
    audioWorklet: async () => {
      await new AudioContext().audioWorklet.addModule(blob(`registerProcessor('p', ${processor})`));
      return 'ran';
    },
    offlineAudioWorklet: async () => {
      const context = new OfflineAudioContext(1, 4096, 44100);
      await context.audioWorklet.addModule(blob(`registerProcessor('p', ${processor})`));
      new AudioWorkletNode(context, 'p').connect(context.destination);
      await context.startRendering();
      return 'ran';
    },
    paintWorklet: async () => {
      await CSS.paintWorklet.addModule(blob('registerPaint("p", class { paint() {} })'));
      return 'ran';
    },
    sharedWorker: () => {
      const port = new SharedWorker('/shared-worker.js').port;
      port.start();
      return answer(port);
    },
    serviceWorker: async () => {
      await navigator.serviceWorker.register('/service-worker.js');
      await navigator.serviceWorker.ready;
      return 'ran';
    },
  };
  const results = {};
  for (const [kind, start] of Object.entries(starts)) {
    const late = new Promise(resolve => setTimeout(() => resolve('not within 5 s'), 5000));
    results[kind] = await Promise.race([start().catch(e => 'threw ' + e), late]);
  }
  return results;
})()"#;

/// The scripts that [`START_WORKERS_JS`] loads by URL.
fn worker_routes() -> Router {
    let script = |source: &'static str| {
        get(move || async move { ([(CONTENT_TYPE, "text/javascript")], source) })
    };
    Router::new()
        .route(
            "/shared-worker.js",
            script("onconnect = e => e.ports[0].postMessage('ran')"),
        )
        .route(
            "/service-worker.js",
            script("addEventListener('install', () => skipWaiting())"),
        )
}

/// Every kind of worker and worklet of a page still starts. Chrome pauses
/// the dedicated workers and the worklets of a target whose session
/// auto-attaches (as each page's does, for its cross-site iframes), until a
/// client attaches and releases them.
#[tokio::test]
async fn a_pages_workers_and_worklets_start() {
    let server = FixtureServer::start_with(worker_routes()).await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    // 127.0.0.1 is a secure context, which worklets and service workers need.
    load(&tab, &server.url("127.0.0.1", "/plain")).await;
    let results = eval(&tab, START_WORKERS_JS).await;
    browser.close().await;
    assert_eq!(
        results,
        json!({
            "dedicatedWorker": "ran",
            "nestedWorker": "ran",
            "audioWorklet": "ran",
            "offlineAudioWorklet": "ran",
            "paintWorklet": "ran",
            "sharedWorker": "ran",
            "serviceWorker": "ran",
        })
    );
}

/// Focus emulation reaches the iframes too: once its page focuses a
/// cross-site iframe, the iframe's document has focus, as in a browser
/// window that has the user's focus. Before that, only the page has.
#[tokio::test]
async fn a_focused_cross_site_iframe_has_focus() {
    let frame = "<!doctype html><script>\
        const report = () => parent.postMessage(document.hasFocus(), '*');\
        onfocus = report;\
        report()</script>";
    let page = |Host(host): Host| async move {
        let frame_url = format!(
            "http://{}/focus-frame",
            host.replace("127.0.0.1", "localhost")
        );
        Html(format!(
            "<!doctype html>\
             <script>window.frameHasFocus = [];\
             onmessage = e => window.frameHasFocus.push(e.data)</script>\
             <iframe src='{frame_url}'></iframe>"
        ))
    };
    let routes = Router::new()
        .route("/focus-page", get(page))
        .route("/focus-frame", get(move || async move { Html(frame) }));
    let server = FixtureServer::start_with(routes).await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/focus-page")).await;
    let reported = |expected: Value| {
        let tab = &tab;
        move || {
            let expected = expected.clone();
            async move { eval(tab, "window.frameHasFocus").await == expected }
        }
    };
    let within = Duration::from_secs(10);
    assert_eventually(
        within,
        "the unfocused iframe reported",
        reported(json!([false])),
    )
    .await;
    assert_eq!(eval(&tab, "document.hasFocus()").await, json!(true));

    eval(&tab, "document.querySelector('iframe').focus()").await;
    let focused = reported(json!([false, true]));
    assert_eventually(within, "the focused iframe reported", focused).await;
    browser.close().await;
}
