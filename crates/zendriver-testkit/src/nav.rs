//! Load a page and read it back without going through zendriver's own waits,
//! for tests whose subject is something else.

use std::time::Duration;

use serde_json::Value;
use zendriver::Tab;

use crate::timing::assert_eventually;

/// Bound on [`wait_loaded`]; the slowest canned page takes about a second.
const LOAD_WITHIN: Duration = Duration::from_secs(10);

/// Wait until `tab` is at `url` and its `load` event has fired, by polling
/// the page itself. `url` must be exactly what `location.href` will say,
/// e.g. `http://127.0.0.1:PORT/plain`, not `http://127.0.0.1:PORT/plain/`.
pub async fn wait_loaded(tab: &Tab, url: &str) {
    assert_eventually(LOAD_WITHIN, &format!("{url} finishes loading"), || async {
        let state: Result<String, _> = tab
            .evaluate_main("location.href + ' ' + document.readyState")
            .await;
        state.is_ok_and(|s| s == format!("{url} complete"))
    })
    .await;
}

/// Navigate `tab` to `url` and wait for its `load` event, as [`wait_loaded`]
/// does.
pub async fn load(tab: &Tab, url: &str) {
    tab.goto(url)
        .await
        .unwrap_or_else(|e| panic!("goto {url}: {e}"));
    wait_loaded(tab, url).await;
}

/// The value of `js` in `tab`'s main world, awaited if it's a promise.
pub async fn eval(tab: &Tab, js: &str) -> Value {
    tab.evaluate_main(js)
        .await
        .unwrap_or_else(|e| panic!("evaluate {js}: {e}"))
}
