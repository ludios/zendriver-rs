//! Shared fixtures for the workspace's real-Chrome tests. Not published.
//!
//! - [`FixtureServer`]: a local HTTP server with canned pages (slow loads,
//!   forms, iframes, downloads, trickled bodies, ...) that logs every
//!   request with timestamps.
//! - [`launch_for_test`]: a headless Chrome on a temporary profile, with
//!   `*.test` hosts ([`SITE_A`], [`SITE_B`]) resolving to the fixture server
//!   as distinct sites, and its `tracing` output captured. Closing it checks
//!   that its whole process tree exited.
//! - [`assert_eventually`] and [`assert_not_before`] for timing, and
//!   [`load`] / [`wait_loaded`] to wait for a page without zendriver's waits.
//! - [`PROBE_PATH`]: the stealth consistency probe, a page that reads the
//!   browser's identity in several realms, and [`disagreements`] to compare
//!   those readings with each other and with the HTTP headers.
//!
//! ```no_run
//! use zendriver::Browser;
//! use zendriver_testkit::{FixtureServer, SITE_A, launch_for_test};
//!
//! # async fn example() {
//! let server = FixtureServer::start().await;
//! let browser = launch_for_test(Browser::builder()).await;
//! browser.main_tab().goto(server.url(SITE_A, "/plain")).await.unwrap();
//! browser.close().await;
//! # }
//! ```

// Panicking with a clear message is how test support reports a failure.
#![allow(clippy::panic)]

mod launch;
mod logs;
mod nav;
mod probe;
mod procs;
mod server;
mod timing;

pub use axum;
pub use launch::{TestBrowser, launch_for_test};
pub use logs::{LogLine, Logs};
pub use nav::{eval, load, wait_loaded};
pub use probe::{
    KNOWN_DISAGREEMENTS, PINNED_HARDWARE, PROBE_JS, PROBE_PATH, check_known_disagreements,
    describe_disagreements, disagreements,
};
pub use procs::ChromeTree;
pub use server::{
    BASIC_AUTH_PASSWORD, BASIC_AUTH_USER, DOWNLOAD_BODY, FixtureServer, HOST_RESOLVER_RULES,
    PROXY_PASSWORD, PROXY_USER, RecordedRequest, SITE_A, SITE_B, trickle_body,
};
pub use timing::{assert_eventually, assert_not_before};
