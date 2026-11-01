//! Connecting to a Chrome that something else launched, against real
//! Chrome.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use zendriver::{Browser, BrowserError, ZendriverError};
use zendriver_testkit::{FixtureServer, launch_for_test};

/// `connect` with the DevTools HTTP address, as Chrome's
/// `--remote-debugging-port` serves it, reaches the browser (BR #1).
#[tokio::test]
async fn connect_by_http_url() {
    let browser = launch_for_test(Browser::builder()).await;
    let inspector = browser.main_tab().inspector_url().expect("inspector url");
    let host_port = inspector
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_else(|| panic!("unexpected inspector url {inspector}"));

    let connected = Browser::connect(format!("http://{host_port}"))
        .await
        .expect("connect by HTTP URL");
    let tab = connected.new_tab().await.expect("new_tab");
    let two: i64 = tab.evaluate_main("1 + 1").await.expect("evaluate_main");
    assert_eq!(two, 2);
    connected.close().await.expect("close the connection");
    browser.close().await;
}

/// `connect` to an HTTP server that isn't Chrome's DevTools fails with an
/// error that names the endpoint and says what the server answered.
#[tokio::test]
async fn connect_to_another_http_server_says_what_it_answered() {
    let server = FixtureServer::start().await;
    let endpoint = format!("http://127.0.0.1:{}", server.port());
    match Browser::connect(endpoint.clone()).await {
        Err(ZendriverError::Browser(BrowserError::ConnectEndpoint {
            endpoint: named,
            reason,
        })) => {
            assert_eq!(named, endpoint);
            assert!(reason.contains("404 Not Found"), "{reason}");
        }
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("connected to the fixture server"),
    }
}
