//! Demonstrates the P5 interception API by blocking any subresource whose
//! URL matches `*/ads/*`, then navigating to a real page.
//!
//! Sequence:
//!   1. Build a [`Browser`] in headless mode.
//!   2. Register a `block` rule on the main tab via the [`InterceptBuilder`]
//!      fluent API; `start(Position::Last).await` puts it at the end of the
//!      tab's interception (`Fetch.enable`, then `Fetch.continueRequest` /
//!      `Fetch.failRequest` per paused request in the background) and
//!      returns once Chrome has applied the rule's pattern. Bind the returned [`InterceptHandle`] — its `Drop`
//!      removes the rule, so letting it go out of scope mid-flow would
//!      silently disable interception.
//!   3. `goto` + `wait_for_load` example.com. The page itself doesn't load
//!      anything under `/ads/`, so no rule fires; the example is here to
//!      show the *shape* of the API, not a hit count. Adapt the URL and
//!      pattern to whatever you actually want to block.
//!   4. Print the page title to prove the navigation succeeded with
//!      interception in the loop.
//!
//! Requires the `interception` cargo feature:
//! `cargo run --example intercept_block_ads --features interception`.

use zendriver::Browser;
use zendriver::interception::Position;

#[tokio::main]
#[allow(clippy::result_large_err)] // example boundary; users wrap in their own Error
async fn main() -> zendriver::Result<()> {
    tracing_subscriber::fmt::init();

    let browser = Browser::builder().headless(true).launch().await?;
    let tab = browser.main_tab();

    // `start()` returns an `InterceptHandle`; binding it keeps the rule in
    // force. Letting it drop would remove the rule.
    let _intercept = tab
        .intercept()
        .block("*/ads/*")?
        .start(Position::Last)
        .await?;

    tab.goto("https://example.com").await?;
    tab.wait_for_load().await?;

    let title = tab.title().await?;
    println!("title = {title:?}");

    browser.close().await?;
    Ok(())
}
