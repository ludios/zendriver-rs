//! The targets that Chrome reports only to a page's own DevTools session:
//! its cross-site iframes, its dedicated workers and its worklets.
//!
//! Attaching those iframes makes Chrome hold back the page's dedicated
//! workers and worklets until zendriver attaches and releases them too.

#![cfg(feature = "integration-tests")]

use serde_json::json;
use zendriver::Browser;
use zendriver_testkit::axum::Router;
use zendriver_testkit::axum::http::header::CONTENT_TYPE;
use zendriver_testkit::axum::routing::get;
use zendriver_testkit::{FixtureServer, eval, launch_for_test, load};

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
