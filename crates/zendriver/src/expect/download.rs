//! [`DownloadExpectation`] + [`MatchedDownload`] + [`crate::Tab::expect_download`]
//! (gated `expect`).
//!
//! A download outlives its tab in Chrome (Chrome even closes a tab that it
//! opened only to download a file), so zendriver follows a download at
//! browser scope, and a [`MatchedDownload`] stays usable after its tab
//! closes:
//!
//! - **Which tab it is from.** [`crate::Tab::expect_download`] waits for the
//!   tab's own `Page.downloadWillBegin`, which Chrome sends to the tab that
//!   the downloading frame belongs to (same-process subframes included).
//! - **How it goes.** One task per browser follows
//!   `Browser.downloadProgress` on the browser's own connection, which goes
//!   on after the tab closes, and keeps what it learns per download guid.
//!   The tab's event and the browser's may come on different sockets (they
//!   do for a tab with its own socket), so either can come first: whichever
//!   does makes the record, and the records of the last 256 finished
//!   downloads are kept, for a claim that comes late.
//! - **Where the file goes.** The first `expect_download` in a browser
//!   context where zendriver set no download behavior yet sends the
//!   context's downloads into zendriver's staging directory (one per
//!   browser), each named by its guid. That directory is deleted once the
//!   [`crate::Browser`] and every [`MatchedDownload`] from it are dropped.
//!   In a context whose downloads go to a directory the caller chose
//!   ([`crate::Tab::set_download_path`]), `expect_download` changes
//!   nothing: the file goes there, under the name Chrome gives it, which
//!   Chrome 138 and later report when the download completes.
//!
//! When the follower may have missed events of the downloads it is
//! following (it fell behind the browser's events, or the browser's
//! connection ended or was replaced), it ends each of them as
//! [`DownloadProgressState::Unknown`].

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::StreamExt;
use serde::Deserialize;
use tempfile::TempDir;
use tokio::sync::{oneshot, watch};
use tokio::time::Sleep;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::warn;
use zendriver_transport::{Connection, SessionEnd, SessionEvents, SessionHandle};

use crate::browser::BrowserInner;
use crate::download_behavior::DownloadMode;
use crate::error::{Result, ZendriverError};

/// Default outer timeout for a [`DownloadExpectation`] — matches the rest of
/// the high-level surface (`wait_for_load`, etc).
const DEFAULT_EXPECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How many finished downloads the follower keeps the records of, for a
/// claim that comes after the download finished.
const FINISHED_KEPT: usize = 256;

/// Where a download is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadProgressState {
    /// Chrome is actively transferring bytes.
    InProgress,
    /// Transfer finished successfully; the file is final.
    Completed,
    /// Transfer was canceled or interrupted (Chrome reports both the same
    /// way): by the user, a network error, the browser context's disposal,
    /// the browser's shutdown, etc.
    Canceled,
    /// zendriver can no longer tell: it may have missed some of the
    /// download's events (it fell behind the browser's events, or the
    /// browser's connection ended or was replaced), so the download may
    /// have finished either way, or may still be going on.
    Unknown,
}

impl DownloadProgressState {
    fn from_cdp(s: &str) -> Self {
        match s {
            "completed" => Self::Completed,
            "canceled" => Self::Canceled,
            // CDP only emits the three above; treat anything else as
            // still-receiving rather than guessing an end.
            _ => Self::InProgress,
        }
    }
}

/// Snapshot of a download's progress at a point in time
/// ([`MatchedDownload::state`]).
#[derive(Debug, Clone, Copy)]
pub struct DownloadState {
    /// Bytes received so far.
    pub received_bytes: u64,
    /// Expected total bytes. `0` until Chrome reports a size (some downloads
    /// don't carry a `Content-Length`).
    pub total_bytes: u64,
    /// Lifecycle marker.
    pub state: DownloadProgressState,
}

impl Default for DownloadState {
    fn default() -> Self {
        Self {
            received_bytes: 0,
            total_bytes: 0,
            state: DownloadProgressState::InProgress,
        }
    }
}

/// What zendriver knows of one download.
#[derive(Clone, Debug, Default)]
struct Progress {
    state: DownloadState,
    /// Where Chrome says the completed file is (Chrome 138 and later).
    file_path: Option<PathBuf>,
}

/// `mutex`, locked; a panic while it was held left nothing half-done.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The downloads one browser's follower knows of.
#[derive(Debug, Default)]
struct Records {
    /// Each download's progress, by guid: those in progress, and the last
    /// [`FINISHED_KEPT`] that finished.
    by_guid: HashMap<String, watch::Sender<Progress>>,
    /// The guids of the finished downloads in `by_guid`, oldest first.
    finished: VecDeque<String>,
    /// How many times events may have been lost, or a finished download's
    /// record forgotten: a download that no event mentioned yet may then
    /// have had all its events before.
    losses: u64,
    /// Whether the follower stopped, so that no event will come.
    ended: bool,
}

impl Records {
    /// The progress of download `guid`, made in progress at its first
    /// mention (by a claim, or by the follower).
    fn of(&mut self, guid: &str) -> &watch::Sender<Progress> {
        self.by_guid
            .entry(guid.to_owned())
            .or_insert_with(|| watch::Sender::new(Progress::default()))
    }

    /// Record `progress`, which Chrome reported, for download `guid`, unless
    /// the download already finished (its end stays).
    fn update(&mut self, guid: &str, progress: Progress) {
        let sender = self.of(guid);
        if sender.borrow().state.state != DownloadProgressState::InProgress {
            return;
        }
        let finished = progress.state.state != DownloadProgressState::InProgress;
        sender.send_replace(progress);
        if finished {
            self.finish(guid);
        }
    }

    /// Subscribe to the progress of download `guid`, for an expectation
    /// registered when [`Records::losses`] was `losses_then`. If no event
    /// mentioned the download yet, and events may have been lost since
    /// then (or the follower stopped), its events may all be gone, so the
    /// download is [`DownloadProgressState::Unknown`].
    fn claim(&mut self, guid: &str, losses_then: u64) -> watch::Receiver<Progress> {
        if let Some(sender) = self.by_guid.get(guid) {
            return sender.subscribe();
        }
        let progress = self.of(guid).subscribe();
        if self.ended || self.losses != losses_then {
            self.lose(guid);
        }
        progress
    }

    /// Keep the record of download `guid`, which just finished, among the
    /// last [`FINISHED_KEPT`].
    fn finish(&mut self, guid: &str) {
        self.finished.push_back(guid.to_owned());
        if self.finished.len() > FINISHED_KEPT {
            if let Some(oldest) = self.finished.pop_front() {
                self.by_guid.remove(&oldest);
                self.losses += 1;
            }
        }
    }

    /// End download `guid` as [`DownloadProgressState::Unknown`] if it is in
    /// progress: some of its events may have been missed.
    fn lose(&mut self, guid: &str) {
        let sender = self.of(guid);
        if sender.borrow().state.state != DownloadProgressState::InProgress {
            return;
        }
        sender.send_modify(|progress| progress.state.state = DownloadProgressState::Unknown);
        self.finish(guid);
    }

    /// End every download in progress as [`DownloadProgressState::Unknown`]:
    /// some events may have been missed.
    fn lose_track(&mut self) {
        self.losses += 1;
        // Only finished records are forgotten, so none of these is.
        let in_progress: Vec<String> = self
            .by_guid
            .iter()
            .filter(|(_, sender)| sender.borrow().state.state == DownloadProgressState::InProgress)
            .map(|(guid, _)| guid.clone())
            .collect();
        for guid in in_progress {
            self.lose(&guid);
        }
    }
}

/// The follower of one browser's downloads.
#[derive(Debug)]
struct Follower {
    records: Arc<Mutex<Records>>,
    /// Stops the follower's task when the browser is dropped.
    _stop: DropGuard,
}

/// The downloads of one browser: zendriver's staging directory, and the
/// task that follows the browser's download events.
#[derive(Debug, Default)]
pub(crate) struct Downloads {
    /// zendriver's staging directory, made at its first use.
    staging: Mutex<Option<Arc<TempDir>>>,
    /// The follower, started at the first [`crate::Tab::expect_download`].
    follower: Mutex<Option<Follower>>,
}

impl Downloads {
    /// zendriver's staging directory, made if it doesn't exist yet.
    fn staging(&self) -> Result<Arc<TempDir>> {
        let mut staging = lock(&self.staging);
        if let Some(dir) = staging.as_ref() {
            return Ok(dir.clone());
        }
        let dir = tempfile::Builder::new()
            .prefix("zendriver-downloads-")
            .tempdir()?;
        let dir = Arc::new(dir);
        *staging = Some(dir.clone());
        Ok(dir)
    }

    /// What the follower knows, after starting it if it isn't running yet
    /// (or has stopped, because the connection's socket died or was shut
    /// down). `conn` is the browser's own connection. The follower sees
    /// every event that Chrome sends after this returns.
    fn records(&self, conn: &Connection) -> Arc<Mutex<Records>> {
        let mut follower = lock(&self.follower);
        if let Some(follower) = follower.as_ref() {
            if !lock(&follower.records).ended {
                return follower.records.clone();
            }
        }
        let records = Arc::new(Mutex::new(Records::default()));
        let stop = CancellationToken::new();
        let session = SessionHandle::new_root(conn.clone());
        let events = session.events(FOLLOWED);
        tokio::spawn(follow(session, events, records.clone(), stop.clone()));
        *follower = Some(Follower {
            records: records.clone(),
            _stop: stop.drop_guard(),
        });
        records
    }
}

/// CDP `Browser.downloadProgress` payload.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadProgressEvent {
    guid: String,
    #[serde(default)]
    received_bytes: u64,
    #[serde(default)]
    total_bytes: u64,
    state: String,
    /// Where the completed file is; Chrome 138 and later.
    #[serde(default)]
    file_path: Option<PathBuf>,
}

/// The events of the browser's own session that the follower reads. (A
/// page session that turned download events on would send copies of its
/// own, which aren't read.)
const FOLLOWED: &[&str] = &["Browser.downloadProgress"];

/// Follow `events`, the browser's own `session`'s subscription to
/// [`FOLLOWED`], into `records`, until `stop` is cancelled or the
/// connection's socket dies or is shut down; then end every download still
/// in progress as [`DownloadProgressState::Unknown`].
async fn follow(
    session: SessionHandle,
    mut events: SessionEvents,
    records: Arc<Mutex<Records>>,
    stop: CancellationToken,
) {
    loop {
        let next = tokio::select! {
            () = stop.cancelled() => None,
            next = events.next() => next,
        };
        match next {
            Some(Ok(event)) => {
                match DownloadProgressEvent::deserialize(&event.params) {
                    Ok(ev) => {
                        let progress = Progress {
                            state: DownloadState {
                                received_bytes: ev.received_bytes,
                                total_bytes: ev.total_bytes,
                                state: DownloadProgressState::from_cdp(&ev.state),
                            },
                            file_path: ev.file_path,
                        };
                        lock(&records).update(&ev.guid, progress);
                    }
                    Err(error) => {
                        // Its download's end may be what was lost.
                        warn!(%error, "an unreadable Browser.downloadProgress");
                        if let Some(guid) = event.params["guid"].as_str() {
                            lock(&records).lose(guid);
                        }
                    }
                }
            }
            // Some downloads' events may be lost: those that overflowed
            // the queue, or those sent while the connection moved to a new
            // socket (where the browser's own session goes on).
            Some(Err(SessionEnd::Overflowed | SessionEnd::Reconnected)) => {
                lock(&records).lose_track();
                events = session.events(FOLLOWED);
            }
            // The socket died or was shut down, or the follower was stopped.
            Some(Err(_)) | None => {
                let mut records = lock(&records);
                records.lose_track();
                records.ended = true;
                return;
            }
        }
    }
}

/// A download observed via its tab's `Page.downloadWillBegin`, which zendriver
/// follows to its end, whether or not the tab is still open.
///
/// Cheap to clone: every clone follows the same download.
#[derive(Clone, Debug)]
pub struct MatchedDownload {
    url: String,
    suggested_filename: String,
    guid: String,
    progress: watch::Receiver<Progress>,
    download_dir: PathBuf,
    /// zendriver's staging directory, kept (and so the file in it) until
    /// every clone is dropped, if Chrome saves the download there, named by
    /// its guid; `None` if it saves the download into a directory the caller
    /// chose, as it suggests.
    staging: Option<Arc<TempDir>>,
}

impl MatchedDownload {
    /// The URL Chrome downloads from.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The file name Chrome suggests (from `Content-Disposition` or the
    /// URL). In a directory the caller chose
    /// ([`crate::Tab::set_download_path`]), the file gets this name, or one
    /// made unique from it.
    #[must_use]
    pub fn suggested_filename(&self) -> &str {
        &self.suggested_filename
    }

    /// Chrome's id of the download. In zendriver's staging directory, it is
    /// also the file's name.
    #[must_use]
    pub fn guid(&self) -> &str {
        &self.guid
    }

    /// The directory Chrome saves the download into: zendriver's staging
    /// directory, or the one that [`crate::Tab::set_download_path`],
    /// [`crate::Browser::set_download_path`] or
    /// [`crate::BrowserBuilder::downloads_dir`] chose for the tab's browser
    /// context.
    #[must_use]
    pub fn download_dir(&self) -> &Path {
        &self.download_dir
    }

    /// The download's progress now.
    #[must_use]
    pub fn state(&self) -> DownloadState {
        self.progress.borrow().state
    }

    /// The completed file, without waiting: `None` while the download is
    /// in progress, if it didn't complete, or if Chrome didn't say where the
    /// file is (Chrome before 138 doesn't, for a download into a directory
    /// the caller chose).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let exp = tab.expect_download().await?;
    /// let dl = exp.await?;
    /// if let Some(p) = dl.path() {
    ///     println!("downloaded to {}", p.display());
    /// }
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn path(&self) -> Option<PathBuf> {
        self.completed_path(&self.progress.borrow())
    }

    /// The completed file of `progress`, a progress of this download.
    fn completed_path(&self, progress: &Progress) -> Option<PathBuf> {
        if progress.state.state != DownloadProgressState::Completed {
            return None;
        }
        let in_staging = || self.download_dir.join(&self.guid);
        progress
            .file_path
            .clone()
            .or_else(|| self.staging.is_some().then(in_staging))
    }

    /// Wait for the download to end, however long it takes (wrap this in
    /// [`tokio::time::timeout`] to bound it); returns the completed file.
    /// Closing the download's tab doesn't end the wait, nor the download.
    ///
    /// # Errors
    ///
    /// [`ZendriverError::Navigation`] if the download was canceled or
    /// interrupted, or if it completed but Chrome didn't say where the file
    /// is (as for [`MatchedDownload::path`]);
    /// [`ZendriverError::EventStreamIncomplete`] if zendriver may have
    /// missed how it ended ([`DownloadProgressState::Unknown`]).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let dl = tab.expect_download().await?.await?;
    /// let file = dl.wait().await?;
    /// println!("downloaded to {}", file.display());
    /// # Ok(()) }
    /// ```
    pub async fn wait(&self) -> Result<PathBuf> {
        let mut progress = self.progress.clone();
        let ended = progress
            .wait_for(|p| p.state.state != DownloadProgressState::InProgress)
            .await;
        let ended = match ended {
            Ok(ended) => ended.clone(),
            // The follower ends a download (as `Unknown`) before it drops
            // the download's record, so a dropped record means a lost one.
            Err(_) => return Err(ZendriverError::EventStreamIncomplete),
        };
        match ended.state.state {
            DownloadProgressState::Completed => self.completed_path(&ended).ok_or_else(|| {
                ZendriverError::Navigation(format!(
                    "download {} completed, but Chrome didn't say where the file is \
                     (Chrome 138 and later usually do)",
                    self.guid
                ))
            }),
            DownloadProgressState::Canceled => Err(ZendriverError::Navigation(format!(
                "download {} was canceled or interrupted",
                self.guid
            ))),
            _ => Err(ZendriverError::EventStreamIncomplete),
        }
    }

    /// Wait for the download to complete (as [`MatchedDownload::wait`]
    /// does), then copy the file to `dest`.
    ///
    /// `dest` is interpreted as a full filename — join a directory +
    /// [`Self::suggested_filename`] if you want to preserve Chrome's name.
    /// If `dest` is the downloaded file itself, nothing is copied.
    ///
    /// # Errors
    ///
    /// Those of [`MatchedDownload::wait`], and [`ZendriverError::Io`] on
    /// file-copy failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let exp = tab.expect_download().await?;
    /// let dl = exp.await?;
    /// dl.save_to("/tmp/downloaded.zip").await?;
    /// # Ok(()) }
    /// ```
    pub async fn save_to(&self, dest: impl AsRef<Path>) -> Result<()> {
        let src = self.wait().await?;
        let dest = dest.as_ref();
        // Copying the file onto itself would empty it.
        let src_itself = tokio::fs::canonicalize(&src).await?;
        let onto_itself = tokio::fs::canonicalize(dest)
            .await
            .is_ok_and(|dest| dest == src_itself);
        if !onto_itself {
            tokio::fs::copy(&src, dest).await?;
        }
        Ok(())
    }
}

/// Awaitable handle returned by [`crate::Tab::expect_download`]. Resolves
/// with the first matched [`MatchedDownload`], [`ZendriverError::Timeout`]
/// if no download begins within the configured timeout, or
/// [`ZendriverError::EventStreamIncomplete`] if the tab's events end or
/// lose continuity first (as when the tab closes).
///
/// Implements [`Future`] directly — `.await` works without calling
/// `.matched()`. The `.matched()` accessor exists for parity with the
/// Playwright-style fluent API.
#[derive(Debug)]
pub struct DownloadExpectation {
    rx: oneshot::Receiver<Result<MatchedDownload>>,
    timeout: Duration,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl DownloadExpectation {
    /// Override the default 30s timeout.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let exp = tab.expect_download().await?.timeout(Duration::from_secs(60));
    /// # let _ = exp;
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn timeout(mut self, dur: Duration) -> Self {
        self.timeout = dur;
        // Reset any already-armed sleep — the next poll will rebuild it
        // with the new deadline.
        self.sleep = None;
        self
    }

    /// Playwright-style alias for `.await`.
    ///
    /// Functionally identical to awaiting the expectation directly.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let dl = tab.expect_download().await?.matched().await?;
    /// # let _ = dl;
    /// # Ok(()) }
    /// ```
    pub async fn matched(self) -> Result<MatchedDownload> {
        self.await
    }
}

impl Future for DownloadExpectation {
    type Output = Result<MatchedDownload>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Poll the oneshot first — if the subscriber already sent, return
        // without ever arming the sleep timer.
        match Pin::new(&mut self.rx).poll(cx) {
            Poll::Ready(Ok(Ok(d))) => return Poll::Ready(Ok(d)),
            // Subscriber observed a delivery-loss boundary (disconnect,
            // shutdown, reconnect, lag, the tab's closing, or a same-method
            // decode failure) before a download began — see
            // `crate::expect::watch`. We can no longer prove a download
            // didn't begin just before the boundary, so reporting a plain
            // timeout would be a lie.
            Poll::Ready(Ok(Err(e))) => return Poll::Ready(Err(e)),
            Poll::Ready(Err(_)) => {
                // Sender dropped without sending anything — the watching
                // task ended with no end and no match observed. The
                // session's events always end with an `Err` saying why,
                // so this arm is defensive. Surface as timeout: same
                // observable shape as a genuine no-show.
                return Poll::Ready(Err(ZendriverError::Timeout(self.timeout)));
            }
            Poll::Pending => {}
        }

        let timeout = self.timeout;
        let sleep = self
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
        match sleep.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(ZendriverError::Timeout(timeout))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// CDP `Page.downloadWillBegin` payload. Field names follow the protocol
/// (camelCase) via serde rename.
#[derive(Debug, Deserialize)]
struct DownloadWillBeginEvent {
    guid: String,
    url: String,
    #[serde(rename = "suggestedFilename", default)]
    suggested_filename: String,
}

/// Start waiting for the next download of the tab whose session is
/// `session`, in browser context `context` (`None` for the default one) of
/// `browser`.
///
/// Before returning: starts following the browser's download events, sends
/// the context's downloads into zendriver's staging directory if zendriver
/// set no download behavior on the context yet, and subscribes to the
/// tab's `Page.downloadWillBegin`, so that a download triggered right after
/// this returns can't slip past.
pub(crate) async fn register(
    browser: &BrowserInner,
    session: &SessionHandle,
    context: Option<&str>,
) -> Result<DownloadExpectation> {
    // Follow the browser's download events before a setting turns them on.
    let downloads = &browser.downloads;
    let records = downloads.records(&browser.conn);
    let staging_mode = || {
        Ok(DownloadMode::Staging(
            downloads.staging()?.path().to_owned(),
        ))
    };
    let mode = browser
        .download_behaviors
        .get_or_set(&browser.conn, context, staging_mode)
        .await?;
    let staging = match mode {
        DownloadMode::Staging(_) => Some(downloads.staging()?),
        DownloadMode::Directory(_) => None,
    };
    let losses_then = lock(&records).losses;
    let mut began =
        crate::expect::watch::<DownloadWillBeginEvent>(session, "Page.downloadWillBegin");
    let (mut tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let next = tokio::select! {
            // The expectation was dropped.
            () = tx.closed() => return,
            next = began.next() => next,
        };
        let Some(began) = next else {
            return;
        };
        let matched = began.map(|ev| {
            let progress = lock(&records).claim(&ev.guid, losses_then);
            MatchedDownload {
                url: ev.url,
                suggested_filename: ev.suggested_filename,
                guid: ev.guid,
                progress,
                download_dir: mode.dir().to_owned(),
                staging,
            }
        });
        let _ = tx.send(matched);
    });
    Ok(DownloadExpectation {
        rx,
        timeout: DEFAULT_EXPECT_TIMEOUT,
        sleep: None,
    })
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use zendriver_transport::testing::MockConnection;

    use crate::Tab;

    /// Far more than the mock needs to deliver an event.
    const PROMPTLY: Duration = Duration::from_secs(2);

    /// `tab.expect_download()`, answering the `Browser.setDownloadBehavior`
    /// it sends, if any, on `mock` (the browser's connection); returns the
    /// expectation and that command's params.
    async fn expect_download(
        mock: &mut MockConnection,
        tab: &Tab,
        sends_a_setting: bool,
    ) -> (DownloadExpectation, Option<serde_json::Value>) {
        let registering = tokio::spawn({
            let tab = tab.clone();
            async move { tab.expect_download().await }
        });
        let mut params = None;
        if sends_a_setting {
            let id = mock.expect_cmd("Browser.setDownloadBehavior").await;
            params = Some(mock.last_sent()["params"].clone());
            mock.reply(id, json!({})).await;
        }
        let expectation = tokio::time::timeout(PROMPTLY, registering)
            .await
            .expect("expect_download did not return")
            .unwrap()
            .unwrap();
        (expectation, params)
    }

    /// Emit the tab's `Page.downloadWillBegin` of download `guid` on `mock`,
    /// for session `S1`.
    async fn begin(mock: &MockConnection, guid: &str) {
        let params = json!({
            "frameId": "F0",
            "guid": guid,
            "url": "https://example.com/report.pdf",
            "suggestedFilename": "report.pdf",
        });
        mock.emit_event_for_session("Page.downloadWillBegin", params, "S1")
            .await;
    }

    /// Emit the browser's `Browser.downloadProgress` of download `guid` on
    /// `mock`, in state `state`, with `extra` params.
    async fn progress(mock: &MockConnection, guid: &str, state: &str, extra: serde_json::Value) {
        let mut params = json!({
            "guid": guid,
            "totalBytes": 10,
            "receivedBytes": if state == "completed" { 10 } else { 4 },
            "state": state,
        });
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        mock.emit_event("Browser.downloadProgress", params).await;
    }

    /// The first `expect_download` in a context sends its downloads into
    /// the staging directory, named by guid, on the browser's connection,
    /// with events on; the matched download follows the browser's progress,
    /// and its file is `<staging>/<guid>` once it completed.
    #[tokio::test]
    async fn a_download_is_followed_into_the_staging_directory() {
        let (mut mock, conn) = MockConnection::pair();
        let inner = crate::browser::test_only_inner_from_conn(conn.clone());
        let (expectation, params) =
            expect_download(&mut mock, inner.main_tab.get().unwrap(), true).await;
        let params = params.unwrap();
        assert_eq!(params["behavior"], "allowAndName");
        assert_eq!(params["eventsEnabled"], true);
        assert!(params.get("browserContextId").is_none(), "{params}");

        begin(&mock, "G1").await;
        let dl = tokio::time::timeout(PROMPTLY, expectation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(dl.guid(), "G1");
        assert_eq!(dl.url(), "https://example.com/report.pdf");
        assert_eq!(dl.suggested_filename(), "report.pdf");
        assert_eq!(dl.download_dir().to_str(), params["downloadPath"].as_str());
        assert_eq!(dl.state().state, DownloadProgressState::InProgress);
        assert!(dl.path().is_none());

        progress(&mock, "G1", "inProgress", json!({})).await;
        progress(&mock, "G1", "completed", json!({})).await;
        let file = tokio::time::timeout(PROMPTLY, dl.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(file, dl.download_dir().join("G1"));
        assert_eq!(dl.path(), Some(file));
        assert_eq!(dl.state().received_bytes, 10);
        conn.shutdown();
    }

    /// The browser's progress and the tab's event come on different
    /// sockets, so a download can complete before its tab's event claims
    /// it; the claim still finds how it ended.
    #[tokio::test]
    async fn a_download_that_completed_before_its_claim_is_found() {
        let (mut mock, conn) = MockConnection::pair();
        let inner = crate::browser::test_only_inner_from_conn(conn.clone());
        let (expectation, _) =
            expect_download(&mut mock, inner.main_tab.get().unwrap(), true).await;

        progress(&mock, "G1", "completed", json!({})).await;
        begin(&mock, "G1").await;
        let dl = tokio::time::timeout(PROMPTLY, expectation)
            .await
            .unwrap()
            .unwrap();
        let file = tokio::time::timeout(PROMPTLY, dl.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(file, dl.download_dir().join("G1"));
        conn.shutdown();
    }

    /// A download goes on after its tab's session detaches (as closing a
    /// tab on the browser's socket makes it do): it is followed through the
    /// browser's events.
    #[tokio::test]
    async fn a_download_is_followed_after_its_tab_closes() {
        let (mut mock, conn) = MockConnection::pair();
        let inner = crate::browser::test_only_inner_from_conn(conn.clone());
        let (expectation, _) =
            expect_download(&mut mock, inner.main_tab.get().unwrap(), true).await;
        begin(&mock, "G1").await;
        let dl = tokio::time::timeout(PROMPTLY, expectation)
            .await
            .unwrap()
            .unwrap();

        mock.emit_event("Target.detachedFromTarget", json!({ "sessionId": "S1" }))
            .await;
        progress(&mock, "G1", "completed", json!({})).await;
        let ended = tokio::time::timeout(PROMPTLY, dl.wait()).await.unwrap();
        assert!(ended.is_ok(), "{ended:?}");
        conn.shutdown();
    }

    /// In a context whose downloads go to a directory the caller chose,
    /// `expect_download` sends no setting, and the file is where Chrome says
    /// (Chrome 138 and later); without that, the wait says Chrome didn't
    /// say.
    #[tokio::test]
    async fn a_download_into_a_chosen_directory_is_where_chrome_says() {
        let (mut mock, conn) = MockConnection::pair();
        let inner = crate::browser::test_only_inner_from_conn(conn.clone());
        let setting = tokio::spawn({
            let tab = inner.main_tab.get().unwrap().clone();
            async move { tab.set_download_path("/tmp/chosen").await }
        });
        let id = mock.expect_cmd("Browser.setDownloadBehavior").await;
        mock.reply(id, json!({})).await;
        setting.await.unwrap().unwrap();

        let (expectation, _) =
            expect_download(&mut mock, inner.main_tab.get().unwrap(), false).await;
        begin(&mock, "G1").await;
        let dl = tokio::time::timeout(PROMPTLY, expectation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(dl.download_dir(), Path::new("/tmp/chosen"));
        let path = json!({ "filePath": "/tmp/chosen/report.pdf" });
        progress(&mock, "G1", "completed", path).await;
        let file = tokio::time::timeout(PROMPTLY, dl.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(file, Path::new("/tmp/chosen/report.pdf"));

        let (expectation, _) =
            expect_download(&mut mock, inner.main_tab.get().unwrap(), false).await;
        begin(&mock, "G2").await;
        let dl = tokio::time::timeout(PROMPTLY, expectation)
            .await
            .unwrap()
            .unwrap();
        progress(&mock, "G2", "completed", json!({})).await;
        let ended = tokio::time::timeout(PROMPTLY, dl.wait()).await.unwrap();
        assert!(
            matches!(ended, Err(ZendriverError::Navigation(_))),
            "{ended:?}"
        );
        conn.shutdown();
    }

    /// A canceled download fails the wait; and once a download ended, a
    /// later event doesn't change how it ended.
    #[tokio::test]
    async fn a_canceled_download_fails_the_wait() {
        let (mut mock, conn) = MockConnection::pair();
        let inner = crate::browser::test_only_inner_from_conn(conn.clone());
        let (expectation, _) =
            expect_download(&mut mock, inner.main_tab.get().unwrap(), true).await;
        begin(&mock, "G1").await;
        let dl = tokio::time::timeout(PROMPTLY, expectation)
            .await
            .unwrap()
            .unwrap();
        progress(&mock, "G1", "canceled", json!({})).await;
        progress(&mock, "G1", "completed", json!({})).await;
        let ended = tokio::time::timeout(PROMPTLY, dl.wait()).await.unwrap();
        assert!(
            matches!(ended, Err(ZendriverError::Navigation(_))),
            "{ended:?}"
        );
        assert_eq!(dl.state().state, DownloadProgressState::Canceled);
        conn.shutdown();
    }

    /// When the browser's connection dies, zendriver may have missed how a
    /// download ended: it ends as `Unknown`, and the wait says so.
    #[tokio::test]
    async fn a_download_in_progress_when_the_connection_dies_is_unknown() {
        let (mut mock, conn) = MockConnection::pair();
        let inner = crate::browser::test_only_inner_from_conn(conn.clone());
        let (expectation, _) =
            expect_download(&mut mock, inner.main_tab.get().unwrap(), true).await;
        begin(&mock, "G1").await;
        let dl = tokio::time::timeout(PROMPTLY, expectation)
            .await
            .unwrap()
            .unwrap();

        mock.disconnect();
        let ended = tokio::time::timeout(PROMPTLY, dl.wait()).await.unwrap();
        assert!(
            matches!(ended, Err(ZendriverError::EventStreamIncomplete)),
            "{ended:?}"
        );
        assert_eq!(dl.state().state, DownloadProgressState::Unknown);
        conn.shutdown();
    }

    /// Dropping the browser ends a download in progress as `Unknown` too:
    /// nothing follows it after that.
    #[tokio::test]
    async fn a_download_in_progress_when_the_browser_drops_is_unknown() {
        let (mut mock, conn) = MockConnection::pair();
        let inner = crate::browser::test_only_inner_from_conn(conn.clone());
        let (expectation, _) =
            expect_download(&mut mock, inner.main_tab.get().unwrap(), true).await;
        begin(&mock, "G1").await;
        let dl = tokio::time::timeout(PROMPTLY, expectation)
            .await
            .unwrap()
            .unwrap();

        drop(inner);
        let ended = tokio::time::timeout(PROMPTLY, dl.wait()).await.unwrap();
        assert!(
            matches!(ended, Err(ZendriverError::EventStreamIncomplete)),
            "{ended:?}"
        );
        assert_eq!(dl.state().state, DownloadProgressState::Unknown);
    }

    /// A download that the tab reports after the follower may have lost
    /// events (here, the browser's connection was replaced), and that no
    /// event mentioned before, may have had all its events then: it is
    /// `Unknown`, instead of waiting forever. The tab has its own socket,
    /// whose events go on.
    #[tokio::test]
    async fn a_download_claimed_after_lost_events_is_unknown() {
        let (mut browser_mock, browser_conn) = MockConnection::pair();
        let (tab_mock, tab_conn) = MockConnection::pair();
        let inner = crate::browser::test_only_inner_from_conn(browser_conn.clone());
        let tab = Tab::new_for_test_in(
            SessionHandle::new_root(tab_conn.clone()),
            Arc::downgrade(&inner),
            "T2",
            None,
        );
        let (expectation, _) = expect_download(&mut browser_mock, &tab, true).await;

        browser_mock.reconnect(&browser_conn);
        // Let the follower see the reconnect before the claim.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let params = json!({
            "frameId": "F0",
            "guid": "G1",
            "url": "https://example.com/report.pdf",
            "suggestedFilename": "report.pdf",
        });
        tab_mock.emit_event("Page.downloadWillBegin", params).await;
        let dl = tokio::time::timeout(PROMPTLY, expectation)
            .await
            .unwrap()
            .unwrap();
        let ended = tokio::time::timeout(PROMPTLY, dl.wait()).await.unwrap();
        assert!(
            matches!(ended, Err(ZendriverError::EventStreamIncomplete)),
            "{ended:?}"
        );
        tab_conn.shutdown();
        browser_conn.shutdown();
    }

    /// A progress event that can't be read may be the download's end: the
    /// download ends as `Unknown`.
    #[tokio::test]
    async fn an_unreadable_progress_ends_its_download_as_unknown() {
        let (mut mock, conn) = MockConnection::pair();
        let inner = crate::browser::test_only_inner_from_conn(conn.clone());
        let (expectation, _) =
            expect_download(&mut mock, inner.main_tab.get().unwrap(), true).await;
        begin(&mock, "G1").await;
        let dl = tokio::time::timeout(PROMPTLY, expectation)
            .await
            .unwrap()
            .unwrap();

        let params = json!({ "guid": "G1", "state": 7 });
        mock.emit_event("Browser.downloadProgress", params).await;
        let ended = tokio::time::timeout(PROMPTLY, dl.wait()).await.unwrap();
        assert!(
            matches!(ended, Err(ZendriverError::EventStreamIncomplete)),
            "{ended:?}"
        );
        conn.shutdown();
    }

    /// Saving a download onto its own file copies nothing: copying it onto
    /// itself would empty it.
    #[tokio::test]
    async fn save_to_onto_the_file_itself_keeps_it() {
        let (mut mock, conn) = MockConnection::pair();
        let inner = crate::browser::test_only_inner_from_conn(conn.clone());
        let (expectation, _) =
            expect_download(&mut mock, inner.main_tab.get().unwrap(), true).await;
        begin(&mock, "G1").await;
        let dl = tokio::time::timeout(PROMPTLY, expectation)
            .await
            .unwrap()
            .unwrap();
        let file = dl.download_dir().join("G1");
        tokio::fs::write(&file, b"data").await.unwrap();
        progress(&mock, "G1", "completed", json!({})).await;

        tokio::time::timeout(PROMPTLY, dl.save_to(&file))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tokio::fs::read(&file).await.unwrap(), b"data");
        conn.shutdown();
    }

    /// A genuine no-show still returns `ZendriverError::Timeout`.
    #[tokio::test]
    async fn expect_download_times_out() {
        let (mut mock, conn) = MockConnection::pair();
        let inner = crate::browser::test_only_inner_from_conn(conn.clone());
        let (expectation, _) =
            expect_download(&mut mock, inner.main_tab.get().unwrap(), true).await;

        let res = expectation.timeout(Duration::from_millis(50)).await;
        match res {
            Err(ZendriverError::Timeout(d)) => {
                assert_eq!(d, Duration::from_millis(50));
            }
            other => panic!("expected Timeout(50ms), got {other:?}"),
        }
        conn.shutdown();
    }

    /// A transport teardown (`SessionEnd::Disconnected`, via
    /// `MockConnection::disconnect`) before any download begins resolves the
    /// wait with `EventStreamIncomplete`, not `Timeout` — we lost the
    /// ability to observe the event, we didn't observe its genuine absence.
    #[tokio::test]
    async fn expect_download_returns_event_stream_incomplete_on_disconnect() {
        let (mut mock, conn) = MockConnection::pair();
        let inner = crate::browser::test_only_inner_from_conn(conn.clone());
        let (expectation, _) =
            expect_download(&mut mock, inner.main_tab.get().unwrap(), true).await;

        mock.disconnect();
        let res = tokio::time::timeout(PROMPTLY, expectation)
            .await
            .expect("expectation did not resolve within 2s after disconnect");
        assert!(
            matches!(res, Err(ZendriverError::EventStreamIncomplete)),
            "expected EventStreamIncomplete after transport teardown, got {res:?}",
        );
        conn.shutdown();
    }
}
