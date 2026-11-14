//! The targets that Chrome reports only to a page's own DevTools session:
//! its cross-site iframes, its dedicated workers and its worklets.
//!
//! A cross-site iframe runs in another renderer process (it is an
//! out-of-process iframe), where the overrides and the script of the page's
//! session don't apply: zendriver has to attach it and set it up like the
//! page, so that the identity it reads is its page's. Without that, a
//! headless launch's cross-site iframes report `HeadlessChrome`, and the
//! host's locale and timezone. These tests serve the page from `127.0.0.1`
//! and its iframes from `localhost`: two sites, and both secure contexts,
//! which `navigator.userAgentData`, `navigator.deviceMemory` and
//! `navigator.geolocation` need.
//!
//! Attaching those iframes makes Chrome hold back the page's dedicated
//! workers and worklets until zendriver attaches and releases them too.

#![cfg(feature = "integration-tests")]

use std::time::Duration;

use serde_json::{Value, json};
use tracing::Level;
use zendriver::stealth::StealthProfile;
use zendriver::{Browser, BrowserBuilder, Persona, Tab};
use zendriver_testkit::axum::Router;
use zendriver_testkit::axum::extract::Host;
use zendriver_testkit::axum::http::header::CONTENT_TYPE;
use zendriver_testkit::axum::response::Html;
use zendriver_testkit::axum::routing::get;
use zendriver_testkit::{
    FixtureServer, PINNED_HARDWARE, TestBrowser, assert_eventually, eval, launch_for_test, load,
};

/// How long a page's iframes get to post what a test waits for.
const WITHIN: Duration = Duration::from_secs(10);

/// A fixture server with `/page`, which has `frames` iframes of `/frame` on
/// `localhost`, and collects in `window.fromFrames` what they post to it.
/// `frame_script` is the script of `/frame`. Both documents have the
/// testkit's `zdIdentity()`.
async fn server(frame_script: &'static str, frames: usize) -> FixtureServer {
    const IDENTITY: &str = "<script src='/probe/identity.js'></script>";
    let page = move |Host(host): Host| async move {
        let frame_url = format!("http://{}/frame", host.replace("127.0.0.1", "localhost"));
        let iframes = format!("<iframe src='{frame_url}'></iframe>").repeat(frames);
        Html(format!(
            "<!doctype html>{IDENTITY}\
             <script>window.fromFrames = [];\
             onmessage = e => window.fromFrames.push(e.data)</script>{iframes}"
        ))
    };
    let frame = Html(format!(
        "<!doctype html>{IDENTITY}<script>{frame_script}</script>"
    ));
    let routes = Router::new()
        .route("/page", get(page))
        .route("/frame", get(move || async move { frame }));
    FixtureServer::start_with(routes).await
}

/// Load `server`'s `/page` in `browser`'s main tab, and return the tab.
async fn load_page(browser: &TestBrowser, server: &FixtureServer) -> Tab {
    let tab = browser.main_tab();
    load(&tab, &server.url("127.0.0.1", "/page")).await;
    tab
}

/// What the iframes of `tab`'s page have posted to it, once there are
/// `count` such messages.
async fn posted(tab: &Tab, count: usize) -> Value {
    let all_posted = || async { eval(tab, "window.fromFrames.length").await == json!(count) };
    assert_eventually(WITHIN, "the iframes posted to their page", all_posted).await;
    eval(tab, "window.fromFrames").await
}

/// The number of `iframe` targets in `browser`: its out-of-process iframes.
async fn iframe_targets(browser: &TestBrowser) -> usize {
    let targets = browser
        .cdp()
        .call_raw("Target.getTargets", json!({}), None)
        .await
        .expect("Target.getTargets");
    let targets = targets["targetInfos"].as_array().expect("targetInfos");
    targets.iter().filter(|t| t["type"] == "iframe").count()
}

/// JS that evaluates to a promise of what a document reads of the browser's
/// identity.
const IDENTITY_JS: &str = "(async () => ({
  identity: await zdIdentity(),
  locale: Intl.DateTimeFormat().resolvedOptions().locale,
}))()";

/// Check that the two cross-site iframes of a page, in a browser launched
/// from `builder`, read the identity that the page reads ([`IDENTITY_JS`]),
/// and return that identity.
async fn identity_shared_with_cross_site_iframes(builder: BrowserBuilder) -> Value {
    const FRAME_SCRIPT: &str = "(async () => ({
        identity: await zdIdentity(),
        locale: Intl.DateTimeFormat().resolvedOptions().locale,
      }))().then(identity => parent.postMessage(identity, '*'))";
    let server = server(FRAME_SCRIPT, 2).await;
    let browser = launch_for_test(builder).await;
    let tab = load_page(&browser, &server).await;
    let frames = posted(&tab, 2).await;
    let page = eval(&tab, IDENTITY_JS).await;
    let out_of_process = iframe_targets(&browser).await;
    browser.logs().assert_none_at_least(Level::WARN);
    browser.close().await;
    assert_eq!(out_of_process, 2, "the iframes must be out-of-process");
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
    let builder = Browser::builder().persona_overlay(german());
    let seen = identity_shared_with_cross_site_iframes(builder).await;
    let user_agent = seen["identity"]["userAgent"].as_str().expect("userAgent");
    assert!(!user_agent.contains("Headless"), "{user_agent}");
    assert_eq!(seen["locale"], "de-DE");
    assert_eq!(seen["identity"]["timeZone"], "Europe/Berlin");
}

/// Under `spoofed`, the bootstrap script reaches the iframes too: it is what
/// reports the persona's `deviceMemory`.
#[tokio::test]
async fn cross_site_iframes_read_the_pages_identity_under_spoofed() {
    let pinned: Persona = PINNED_HARDWARE.parse().expect("persona JSON");
    let builder = Browser::builder()
        .stealth(StealthProfile::spoofed())
        .persona_overlay(german().overlay(pinned));
    let seen = identity_shared_with_cross_site_iframes(builder).await;
    assert_eq!(seen["identity"]["hardwareConcurrency"], 1);
    assert_eq!(seen["identity"]["deviceMemory"], 1);
    assert_eq!(seen["identity"]["timeZone"], "Europe/Berlin");
}

/// Focus emulation reaches the iframes too: once its page focuses a
/// cross-site iframe, the iframe's document has focus, as in a browser
/// window that has the user's focus. Before that, only the page has.
#[tokio::test]
async fn a_focused_cross_site_iframe_has_focus() {
    const FRAME_SCRIPT: &str = "const report = () => parent.postMessage(document.hasFocus(), '*');
        onfocus = report;
        report()";
    let server = server(FRAME_SCRIPT, 1).await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = load_page(&browser, &server).await;
    assert_eq!(posted(&tab, 1).await, json!([false]));
    assert_eq!(eval(&tab, "document.hasFocus()").await, json!(true));
    assert_eq!(iframe_targets(&browser).await, 1);

    eval(&tab, "document.querySelector('iframe').focus()").await;
    assert_eq!(posted(&tab, 2).await, json!([false, true]));
    browser.close().await;
}

/// The geolocation override is the tab's, and Chrome clears it when the
/// session that set it ends. So only the page's session may set it: the end
/// of a cross-site iframe must leave it in place.
#[tokio::test]
async fn removing_a_cross_site_iframe_keeps_the_tabs_geolocation() {
    const POSITION_JS: &str = "new Promise(resolve => navigator.geolocation.getCurrentPosition(
        position => resolve([position.coords.latitude, position.coords.longitude]),
        error => resolve(error.message),
        {timeout: 5000}))";
    let berlin: Persona =
        r#"{"geolocation": {"latitude": 52.52, "longitude": 13.405, "accuracy": 10.0}}"#
            .parse()
            .expect("persona JSON");
    let server = server("parent.postMessage('loaded', '*')", 1).await;
    let browser = launch_for_test(Browser::builder().persona_overlay(berlin)).await;
    let granted = json!({ "permissions": ["geolocation"] });
    browser
        .cdp()
        .call_raw("Browser.grantPermissions", granted, None)
        .await
        .expect("Browser.grantPermissions");
    let tab = load_page(&browser, &server).await;
    posted(&tab, 1).await;
    assert_eq!(eval(&tab, POSITION_JS).await, json!([52.52, 13.405]));

    eval(&tab, "document.querySelector('iframe').remove()").await;
    let gone = || async { iframe_targets(&browser).await == 0 };
    assert_eventually(WITHIN, "the iframe's target is gone", gone).await;
    assert_eq!(eval(&tab, POSITION_JS).await, json!([52.52, 13.405]));
    browser.close().await;
}

/// A renderer process holds one timezone override for all of its targets,
/// and drops it when the session that set it ends. So when the first of a
/// site's two iframes is removed, the second falls back to the timezone that
/// Chrome started in. On Unix, a launch starts Chrome in the persona's
/// timezone (as `TZ`), so the second iframe's timezone stays the persona's.
#[cfg(unix)]
#[tokio::test]
async fn removing_a_cross_site_iframe_keeps_its_siblings_timezone() {
    // A zone that no host of these tests is in.
    const ZONE: &str = "Pacific/Kiritimati";
    const FRAME_SCRIPT: &str = "
        const zone = () => Intl.DateTimeFormat().resolvedOptions().timeZone;
        parent.postMessage(zone(), '*');
        onmessage = () => parent.postMessage(zone(), '*');";
    let persona: Persona = format!(r#"{{"timezone": "{ZONE}"}}"#)
        .parse()
        .expect("persona JSON");
    let server = server(FRAME_SCRIPT, 2).await;
    let browser = launch_for_test(Browser::builder().persona_overlay(persona)).await;
    let tab = load_page(&browser, &server).await;
    assert_eq!(posted(&tab, 2).await, json!([ZONE, ZONE]));

    eval(&tab, "document.querySelector('iframe').remove()").await;
    let one_left = || async { iframe_targets(&browser).await == 1 };
    assert_eventually(WITHIN, "the first iframe's target is gone", one_left).await;
    let ask = "document.querySelector('iframe').contentWindow.postMessage('zone', '*')";
    eval(&tab, ask).await;
    assert_eq!(posted(&tab, 3).await[2], json!(ZONE));
    browser.close().await;
}

/// `force_open_shadow_roots` reaches a cross-site iframe: the shadow root
/// that its script asks to be closed is open.
#[tokio::test]
async fn force_open_shadow_roots_reaches_cross_site_iframes() {
    const FRAME_SCRIPT: &str = "const host = document.createElement('div');
        host.attachShadow({mode: 'closed'});
        parent.postMessage(host.shadowRoot !== null, '*')";
    let server = server(FRAME_SCRIPT, 1).await;
    let browser = launch_for_test(Browser::builder().force_open_shadow_roots(true)).await;
    let tab = load_page(&browser, &server).await;
    let opened = posted(&tab, 1).await;
    let out_of_process = iframe_targets(&browser).await;
    browser.close().await;
    assert_eq!(out_of_process, 1, "the iframe must be out-of-process");
    assert_eq!(opened, json!([true]));
}

/// JS that evaluates to a promise of `{kind: "ran"}` for each kind of worker
/// and worklet that a page can start, or `{kind: why}` for one that didn't
/// run within 5 seconds. It loads the scripts of [`worker_routes`].
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
