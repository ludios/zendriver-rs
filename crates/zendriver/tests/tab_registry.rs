//! The tab registry (`Browser::tabs`) against real Chrome and the fixture
//! server.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic, clippy::unwrap_used)]

use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zendriver::{Browser, ObserverError, PausedSession, TargetObserver};
use zendriver_testkit::{FixtureServer, SITE_A, eval, launch_for_test, load};

/// The registry holds one tab per target, and the main tab's is the main
/// tab itself: when `launch` attached the main target a second time, under
/// another session id, the registry once got a second `Tab` for it, with a
/// socket, trackers and interception of its own.
#[tokio::test]
async fn the_main_target_has_one_tab_the_main_tab() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let main = browser.main_tab();
    load(&main, &server.url(SITE_A, "/plain")).await;

    let tabs = browser.tabs().await;
    assert_eq!(tabs.len(), 1, "{} tabs", tabs.len());
    assert_eq!(tabs[0].target_id(), main.target_id());
    assert!(
        std::ptr::eq(tabs[0].session(), main.session()),
        "the registry's tab for the main target is another Tab than main_tab()"
    );
    browser.close().await;
}

/// An observer that, on each page target, registers a script that counts its
/// runs in each document, after a delay that a launch which doesn't wait for
/// the observer chain would not sit out.
#[derive(Debug, Default)]
struct CountRuns {
    /// For each target id: how many runs of this observer started, and how
    /// many finished.
    runs: Mutex<HashMap<String, (u32, u32)>>,
}

#[async_trait::async_trait]
impl TargetObserver for CountRuns {
    fn name(&self) -> &'static str {
        "count-runs"
    }

    async fn on_target_attached(&self, session: PausedSession<'_>) -> Result<(), ObserverError> {
        if session.target_info.kind != "page" {
            return Ok(());
        }
        let target_id = session.target_info.target_id.clone();
        self.runs
            .lock()
            .unwrap()
            .entry(target_id.clone())
            .or_default()
            .0 += 1;
        tokio::time::sleep(Duration::from_millis(300)).await;
        session.call("Page.enable", json!({})).await?;
        let source = "window.__countRunsScript = (window.__countRunsScript ?? 0) + 1";
        session
            .call(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": source }),
            )
            .await?;
        self.runs.lock().unwrap().entry(target_id).or_default().1 += 1;
        Ok(())
    }
}

/// The observer chain runs once on the main target, and has finished when
/// `launch` returns, so a script that an observer registers runs once in
/// each document of the main tab. (Chrome's auto-attach attaches the main
/// target; when `launch` attached it once more, the chain ran twice, and
/// neither run was waited for.)
#[tokio::test]
async fn observers_run_once_on_the_main_target_before_launch_returns() {
    let server = FixtureServer::start().await;
    let observer = Arc::new(CountRuns::default());
    let browser = launch_for_test(Browser::builder().observer(observer.clone())).await;
    let main = browser.main_tab();
    let runs = observer.runs.lock().unwrap().get(main.target_id()).copied();
    assert_eq!(
        runs,
        Some((1, 1)),
        "(started, finished) runs of the observer"
    );

    load(&main, &server.url(SITE_A, "/plain")).await;
    assert_eq!(eval(&main, "window.__countRunsScript").await, json!(1));
    browser.close().await;
}
