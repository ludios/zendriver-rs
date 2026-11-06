//! Direct file downloads to a custom directory.
//!
//! Equivalent to the upstream zendriver Python feature request in
//! cdpdriver/zendriver#88. `BrowserBuilder::downloads_dir(path)` sends
//! `Browser.setDownloadBehavior {behavior:"allow", downloadPath:...}` for
//! Chrome's default browser context right after launch, so every tab in it
//! — including ones opened later via `Browser::new_tab` — saves files into
//! the configured directory. (Chrome keeps the setting per browser context:
//! a `BrowserContext`'s tabs need `Tab::set_download_path`.)
//!
//! The directory must exist before launch; this example creates a tempdir.

use std::path::PathBuf;

#[tokio::main]
#[allow(clippy::result_large_err)]
async fn main() -> zendriver::Result<()> {
    tracing_subscriber::fmt::init();

    let tmp = tempfile::tempdir().expect("tempdir");
    let dir: PathBuf = tmp.path().to_path_buf();
    println!("downloads will land in: {}", dir.display());

    let browser = zendriver::Browser::builder()
        .headless(true)
        .downloads_dir(&dir)
        .launch()
        .await?;

    let tab = browser.main_tab();
    // Navigate somewhere that triggers a download; left to the user. The
    // setDownloadBehavior call already fired in launch, so any download from
    // a tab of the default context lands in `dir`.
    tab.goto("https://example.com").await?;

    browser.close().await?;
    Ok(())
}
