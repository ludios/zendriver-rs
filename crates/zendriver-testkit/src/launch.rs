//! Launch a real Chrome for a test, and check on close that it's all gone.

use std::ops::Deref;
use std::time::Duration;

use tokio::runtime::{Handle, RuntimeFlavor};
use tracing::subscriber::DefaultGuard;
use zendriver::{Browser, BrowserBuilder};

use crate::logs::Logs;
use crate::procs::ChromeTree;
use crate::server::HOST_RESOLVER_RULES;

/// How long after `close()` returns Chrome's processes get to exit. Helpers
/// are reaped by the kernel shortly after the browser process; this is slack
/// for a loaded CI runner, not an expected wait.
const EXIT_GRACE: Duration = Duration::from_secs(10);

/// A launched Chrome. Derefs to [`Browser`].
///
/// Must be finished with [`TestBrowser::close`], which also checks that every
/// process of this Chrome exited; dropping it unclosed panics (unless the
/// test is already panicking). So fail a test by panicking (`expect`), not
/// by returning `Err`: the early return would drop this unclosed, and that
/// panic would replace the error.
pub struct TestBrowser {
    browser: Browser,
    tree: ChromeTree,
    logs: Logs,
    _log_guard: DefaultGuard,
    closed: bool,
}

/// Launch `builder` headless, with every `*.test` host resolving to the
/// fixture server, and capture this thread's `tracing` output from here
/// until the browser is closed. Chrome gets a fresh temporary profile
/// unless `builder` names one.
///
/// Panics if the launch fails, or if not on a current-thread runtime
/// (`#[tokio::test]`'s default): the log capture is per-thread, and on a
/// multi-thread runtime it would quietly miss events. With two browsers in
/// one test, the later one's capture takes over from the earlier one's.
pub async fn launch_for_test(builder: BrowserBuilder) -> TestBrowser {
    assert_eq!(
        Handle::current().runtime_flavor(),
        RuntimeFlavor::CurrentThread,
        "launch_for_test needs a current-thread runtime, or the log capture misses events",
    );
    let logs = Logs::default();
    let log_guard = logs.capture();
    let browser = builder
        .headless(true)
        .arg(HOST_RESOLVER_RULES)
        .launch()
        .await
        .unwrap_or_else(|e| panic!("launch_for_test: Chrome didn't launch: {e}"));
    let tree = ChromeTree::of(&browser).await;
    TestBrowser {
        browser,
        tree,
        logs,
        _log_guard: log_guard,
        closed: false,
    }
}

impl TestBrowser {
    /// What's been logged since launch.
    pub fn logs(&self) -> &Logs {
        &self.logs
    }

    /// Close the browser, then panic unless all of its processes exit
    /// within a few seconds.
    pub async fn close(mut self) {
        let live = self.tree.snapshot(&self.browser).await;
        assert!(
            live > 0,
            "found none of this Chrome's processes while it was running, so the exit check \
             below would pass without checking anything"
        );
        self.browser.clone().close().await.expect("Browser::close");
        if let Err(survivors) = self.tree.wait_gone(EXIT_GRACE).await {
            panic!(
                "{} Chrome process(es) still running {EXIT_GRACE:?} after close:\n  {}",
                survivors.len(),
                survivors.join("\n  "),
            );
        }
        self.closed = true;
    }
}

impl Deref for TestBrowser {
    type Target = Browser;

    fn deref(&self) -> &Browser {
        &self.browser
    }
}

impl Drop for TestBrowser {
    fn drop(&mut self) {
        if !self.closed && !std::thread::panicking() {
            panic!("TestBrowser dropped without close(), so nothing checked that Chrome exited");
        }
    }
}
