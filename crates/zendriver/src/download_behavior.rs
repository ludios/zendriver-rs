//! The download behavior of each browser context, which zendriver sets in
//! one place.
//!
//! Chrome keeps one download behavior per browser context, and the last
//! `Browser.setDownloadBehavior` for a context wins, whichever session sent
//! it. A setting sent on a tab's own socket belongs to that tab's session,
//! so Chrome drops it when the tab closes; one sent without
//! `browserContextId` goes to the default context, whatever context the
//! sender's tab is in; and one sent without `eventsEnabled: true` turns off
//! the sending session's download events. So zendriver sends every setting
//! through [`DownloadBehaviors`]: on the browser's own connection, naming
//! the context, with `eventsEnabled: true`; and it remembers what it set.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::json;
use zendriver_transport::Connection;

use crate::error::Result;

/// Where a browser context's downloads go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DownloadMode {
    /// Into a directory the caller chose, under the names Chrome gives them
    /// (behavior `allow`).
    Directory(PathBuf),
    /// Into zendriver's staging directory, each named by its guid (behavior
    /// `allowAndName`), for [`crate::Tab::expect_download`] to hand out.
    #[cfg(feature = "expect")]
    Staging(PathBuf),
}

impl DownloadMode {
    /// The directory the downloads go into.
    pub(crate) fn dir(&self) -> &Path {
        match self {
            DownloadMode::Directory(dir) => dir,
            #[cfg(feature = "expect")]
            DownloadMode::Staging(dir) => dir,
        }
    }
}

/// The download behavior that zendriver set on each browser context of one
/// browser.
#[derive(Debug, Default)]
pub(crate) struct DownloadBehaviors {
    /// Each context's mode, by the context's id (`None` for the default
    /// context). Locked across each setting's round trip, so that settings
    /// reach Chrome in the order they are recorded.
    modes: tokio::sync::Mutex<HashMap<Option<String>, DownloadMode>>,
}

impl DownloadBehaviors {
    /// Make the downloads of browser context `context` (`None` for the
    /// default context) go as `mode` says, by sending
    /// `Browser.setDownloadBehavior` on `conn`, the browser's own
    /// connection.
    pub(crate) async fn set(
        &self,
        conn: &Connection,
        context: Option<&str>,
        mode: DownloadMode,
    ) -> Result<()> {
        let mut modes = self.modes.lock().await;
        send(conn, context, &mode).await?;
        modes.insert(context.map(str::to_owned), mode);
        Ok(())
    }

    /// The mode of `context`, after setting it like [`DownloadBehaviors::set`]
    /// if zendriver set nothing on `context` yet; `make_mode` then makes the
    /// mode (it may fail, for example creating a directory).
    pub(crate) async fn get_or_set(
        &self,
        conn: &Connection,
        context: Option<&str>,
        make_mode: impl FnOnce() -> Result<DownloadMode>,
    ) -> Result<DownloadMode> {
        let mut modes = self.modes.lock().await;
        let key = context.map(str::to_owned);
        if let Some(mode) = modes.get(&key) {
            return Ok(mode.clone());
        }
        let mode = make_mode()?;
        send(conn, context, &mode).await?;
        modes.insert(key, mode.clone());
        Ok(mode)
    }

    /// Forget every setting. Chrome drops a session's settings when the
    /// session ends, so this is for when the browser's connection was
    /// replaced.
    pub(crate) async fn forget_all(&self) {
        self.modes.lock().await.clear();
    }
}

/// Send `Browser.setDownloadBehavior` for `mode` on `conn`, naming browser
/// context `context` (`None` for the default context).
async fn send(conn: &Connection, context: Option<&str>, mode: &DownloadMode) -> Result<()> {
    let behavior = match mode {
        DownloadMode::Directory(_) => "allow",
        #[cfg(feature = "expect")]
        DownloadMode::Staging(_) => "allowAndName",
    };
    let mut params = json!({
        "behavior": behavior,
        "downloadPath": mode.dir().to_string_lossy(),
        "eventsEnabled": true,
    });
    // Leaving the id out names the default context, whose tabs record no id
    // (see `BrowserInner::default_context_id`).
    if let Some(context) = context {
        params["browserContextId"] = json!(context);
    }
    conn.call_raw("Browser.setDownloadBehavior", params, None)
        .await?;
    Ok(())
}
