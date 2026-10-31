//! Whether one frame is loading, followed from Chrome's
//! `Page.frameStartedLoading` / `Page.frameStoppedLoading` — internal.
//!
//! Chrome (checked on 156) sends that pair for every kind of navigation of
//! a frame: cross-document, same-document (fragment, `pushState`),
//! back/forward (including a back-forward-cache restore), reload, a
//! download, and an error page. So the frame is loading exactly when a
//! start has arrived since the last stop.
//!
//! The events come from a broadcast channel, which never replays an event
//! sent before subscribing: subscribe with [`loading_changes`] before the
//! action that navigates.

use std::time::Duration;

use futures::{Stream, StreamExt};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zendriver_transport::{RawEvent, SessionHandle};

use crate::error::{Result, ZendriverError};

/// The loading starts and stops of frame `frame_id` that `session` receives
/// from now on, in the order Chrome sent them: `true` for a start, `false`
/// for a stop.
pub(crate) fn loading_changes(
    session: &SessionHandle,
    frame_id: &str,
) -> impl Stream<Item = bool> + Send + Unpin + 'static {
    let session_id = session.session_id().map(str::to_owned);
    let frame_id = frame_id.to_owned();
    // One subscription for both methods, so a start and a stop can't
    // swap places on the way here.
    Box::pin(
        session
            .connection()
            .subscribe_raw()
            .filter_map(move |ev: RawEvent| {
                let mut change = None;
                if ev.session_id == session_id
                    && ev.params.get("frameId").and_then(|v| v.as_str()) == Some(&frame_id)
                {
                    change = match ev.method.as_str() {
                        "Page.frameStartedLoading" => Some(true),
                        "Page.frameStoppedLoading" => Some(false),
                        _ => None,
                    };
                }
                async move { change }
            }),
    )
}

/// Follows whether one frame is loading, in a task that runs until the
/// tracker is dropped.
#[derive(Debug)]
pub(crate) struct LoadTracker {
    loading: watch::Receiver<bool>,
    task: JoinHandle<()>,
}

impl LoadTracker {
    /// Follow `changes` (from [`loading_changes`]). `loading` is whether the
    /// frame counts as loading until the first change arrives.
    pub(crate) fn new(
        mut changes: impl Stream<Item = bool> + Send + Unpin + 'static,
        loading: bool,
    ) -> Self {
        let (tx, rx) = watch::channel(loading);
        let task = tokio::spawn(async move {
            while let Some(loading) = changes.next().await {
                tx.send_replace(loading);
            }
        });
        Self { loading: rx, task }
    }

    /// Follow frame `frame_id` on `session`, counting it as loading until
    /// Chrome says it stopped: for a caller that is about to navigate it.
    pub(crate) fn before_navigating(session: &SessionHandle, frame_id: &str) -> Self {
        Self::new(loading_changes(session, frame_id), true)
    }

    /// Wait until the frame isn't loading.
    ///
    /// # Errors
    ///
    /// [`ZendriverError::Timeout`] if it is still loading after `within`;
    /// [`ZendriverError::Navigation`] if the session's events end first.
    pub(crate) async fn wait(mut self, within: Duration) -> Result<()> {
        match tokio::time::timeout(within, self.loading.wait_for(|loading| !loading)).await {
            Err(_) => Err(ZendriverError::Timeout(within)),
            Ok(Err(_)) => Err(ZendriverError::Navigation(
                "page event stream closed".into(),
            )),
            Ok(Ok(_)) => Ok(()),
        }
    }
}

impl Drop for LoadTracker {
    fn drop(&mut self) {
        self.task.abort();
    }
}
