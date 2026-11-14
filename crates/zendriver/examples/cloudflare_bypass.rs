//! Demonstrates the P5 Cloudflare Turnstile bypass driver.
//!
//! Sequence:
//!   1. Launch a headless browser and navigate to a Cloudflare-protected
//!      URL. Set this to whatever endpoint you actually want to clear.
//!   2. Call [`Tab::cloudflare`] to construct a [`CloudflareBypass`] bound
//!      to the tab's session.
//!   3. Call [`CloudflareBypass::wait_for_clearance`] with a 30s budget.
//!      The driver runs a single CDP poll loop that, per tick, looks for
//!      the clearance token, any challenge marker on the page, and the
//!      Turnstile iframe — through CDP, since Turnstile mounts it in a
//!      closed shadow root. When the iframe is a click target, the driver
//!      scrolls it into view, measures it again, and clicks its default point
//!      (7% across, 50% down: the Turnstile checkbox) in a human-like way —
//!      a curved, eased approach, a little jiggle, a held press — retrying
//!      up to three times in case the widget swallowed the first one.
//!      Resolves the first tick a token is observed, or when every marker
//!      is gone from a page that has settled.
//!   4. Print the [`ClearanceOutcome`] enum variant + the page title.
//!
//! Which markers to look for and how to click are caller data —
//! `.selectors(TurnstileSelectors { .. })`,
//! `.click_policy(ClickPolicy { .. })` and `.on_click(..)` — so a change on
//! Cloudflare's side is a change here, not a release of the crate. This
//! example takes the defaults.
//!
//! Outcomes:
//!   - `AlreadyClear` — the page showed no challenge marker when first
//!     seen parsed: no Turnstile challenge found (none was served, or a
//!     clearance cookie skipped it). Returned at once. See the variant's
//!     docs for what it does not catch.
//!   - `TokenAcquired(_)` — Turnstile yielded a `cf-turnstile-response`
//!     token, either after clicking the checkbox or directly (invisible
//!     Turnstile, where the iframe never mounts).
//!   - `ChallengeGone` — every marker seen earlier is gone without a
//!     token, and the page has settled: Cloudflare's full-page challenge
//!     navigated to the page it guards and that page has been parsed, or
//!     the markers stayed gone for three seconds on the same page. Not by
//!     itself proof the gate was passed.
//!   - `TimedOut { saw_challenge: false }` — the run never saw the page
//!     parsed: it was still loading, or changing, at every look.
//!   - `TimedOut { saw_challenge: true }` — 30s elapsed with markers
//!     present but neither success state observed.
//!
//! Requires the `cloudflare` cargo feature:
//! `cargo run --example cloudflare_bypass --features cloudflare`.

use std::time::Duration;

use zendriver::{Browser, ClearanceOutcome};

#[tokio::main]
#[allow(clippy::result_large_err)] // example boundary; users wrap in their own Error
async fn main() -> zendriver::Result<()> {
    tracing_subscriber::fmt::init();

    let browser = Browser::builder().headless(true).launch().await?;
    let tab = browser.main_tab();

    tab.goto("https://nopecha.com/demo/cloudflare").await?;
    tab.wait_for_load().await?;

    match tab
        .cloudflare()
        .wait_for_clearance(Duration::from_secs(30))
        .await
    {
        Ok(ClearanceOutcome::AlreadyClear) => {
            println!("no challenge marker on this page");
        }
        Ok(ClearanceOutcome::TimedOut {
            saw_challenge: false,
        }) => {
            println!("the page never settled into one parsed document within 30s");
        }
        Ok(ClearanceOutcome::TimedOut {
            saw_challenge: true,
        }) => {
            println!("clearance timed out within 30s");
        }
        Ok(outcome) => println!("cleared: {outcome:?}"),
        Err(e) => return Err(e.into()),
    }

    // `document.title` rather than `Tab::title`, which asks the browser:
    // right after the challenge page is replaced, its copy can still be the
    // challenge page's title.
    let title: String = tab.evaluate("document.title").await?;
    println!("title = {title:?}");

    browser.close().await?;
    Ok(())
}
