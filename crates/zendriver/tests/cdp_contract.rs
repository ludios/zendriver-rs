//! The CDP contract: every command the library sends exists where it sends
//! it, and every event it listens for exists.
//!
//! The mock CDP answers whatever it's asked, so a command that Chrome
//! doesn't have, or doesn't have at that scope, passes every mock test and
//! fails only against Chrome (`-32601`, as with the browser-level cookie
//! methods). Each row of [`COMMANDS`] is sent to a real Chrome at its scope
//! with one parameter of the wrong type, which Chrome rejects (`-32602`)
//! before running the command, but only after finding it. A command with no
//! parameters is sent as is.
//!
//! The table has to name every CDP method in the library: a CDP name in
//! quotes anywhere under `crates/*/src` (the test kit aside), mock tests
//! included, needs a row.

#![cfg(feature = "integration-tests")]
#![allow(clippy::panic)]

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use regex::Regex;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zendriver::{Browser, CallError, Tab};
use zendriver_testkit::{FixtureServer, SITE_A, TestBrowser, launch_for_test};

/// Where the library sends a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    /// The browser target: no session, on the browser's own connection.
    Browser,
    /// A page target: a tab's session, or no session on a tab's own socket.
    Page,
}

use Scope::{Browser as B, Page as P};

/// Every CDP command the library sends, and where. A command sent at both
/// scopes has two rows.
///
/// A call with no session on `session.connection()` goes to the page when
/// the tab has its own socket (the usual case), and to the browser when it
/// fell back to the shared one; such commands are listed at both.
const COMMANDS: &[(&str, Scope)] = &[
    ("Accessibility.getPartialAXTree", P),
    ("Browser.close", B),
    ("Browser.getVersion", B),
    ("Browser.getWindowForTarget", B),
    ("Browser.getWindowForTarget", P),
    ("Browser.grantPermissions", B),
    ("Browser.resetPermissions", B),
    ("Browser.setDownloadBehavior", B),
    ("Browser.setWindowBounds", B),
    ("Browser.setWindowBounds", P),
    ("DOM.describeNode", P),
    ("DOM.getBoxModel", P),
    ("DOM.getDocument", P),
    ("DOM.getOuterHTML", P),
    ("DOM.resolveNode", P),
    ("DOM.setFileInputFiles", P),
    ("DOMStorage.clear", P),
    ("DOMStorage.enable", P),
    ("DOMStorage.getDOMStorageItems", P),
    ("DOMStorage.removeDOMStorageItem", P),
    ("DOMStorage.setDOMStorageItem", P),
    ("Emulation.setDeviceMetricsOverride", P),
    ("Emulation.setFocusEmulationEnabled", P),
    ("Emulation.setHardwareConcurrencyOverride", P),
    ("Emulation.setGeolocationOverride", P),
    ("Emulation.setLocaleOverride", P),
    ("Emulation.setTimezoneOverride", P),
    ("Emulation.setUserAgentOverride", P),
    ("Fetch.continueRequest", P),
    ("Fetch.continueResponse", P),
    ("Fetch.continueWithAuth", P),
    ("Fetch.disable", P),
    ("Fetch.enable", P),
    ("Fetch.failRequest", P),
    ("Fetch.fulfillRequest", P),
    ("Fetch.getResponseBody", P),
    ("IO.close", P),
    ("IO.read", P),
    ("Input.dispatchKeyEvent", P),
    ("Input.dispatchMouseEvent", P),
    ("Input.dispatchTouchEvent", P),
    ("Input.synthesizeScrollGesture", P),
    ("Network.enable", P),
    ("Network.getResponseBody", P),
    ("Network.loadNetworkResource", P),
    ("Network.setCookie", P),
    ("Network.streamResourceContent", P),
    ("Overlay.enable", P),
    ("Overlay.highlightNode", P),
    ("Page.addScriptToEvaluateOnNewDocument", P),
    ("Page.bringToFront", P),
    ("Page.captureScreenshot", P),
    ("Page.captureSnapshot", P),
    ("Page.createIsolatedWorld", P),
    ("Page.enable", P),
    ("Page.getFrameTree", P),
    ("Page.getLayoutMetrics", P),
    ("Page.getNavigationHistory", P),
    ("Page.getResourceTree", P),
    ("Page.handleJavaScriptDialog", P),
    ("Page.navigate", P),
    ("Page.navigateToHistoryEntry", P),
    ("Page.printToPDF", P),
    ("Page.reload", P),
    ("Page.searchInResource", P),
    ("Page.setBypassCSP", P),
    ("Page.setInterceptFileChooserDialog", P),
    ("Runtime.callFunctionOn", P),
    ("Runtime.evaluate", P),
    ("Runtime.getProperties", P),
    ("Runtime.releaseObject", P),
    ("Runtime.runIfWaitingForDebugger", P),
    ("Storage.clearCookies", B),
    ("Storage.clearCookies", P),
    ("Storage.getCookies", B),
    ("Storage.getCookies", P),
    ("Storage.setCookies", B),
    ("Storage.setCookies", P),
    ("SystemInfo.getInfo", B),
    ("Target.activateTarget", B),
    ("Target.attachToTarget", B),
    ("Target.closeTarget", B),
    ("Target.createBrowserContext", B),
    ("Target.createTarget", B),
    ("Target.detachFromTarget", B),
    ("Target.disposeBrowserContext", B),
    ("Target.getBrowserContexts", B),
    ("Target.getTargetInfo", P),
    ("Target.getTargets", B),
    ("Target.setAutoAttach", B),
];

/// Commands the library sends where Chrome doesn't have them: known bugs,
/// each naming its TODO item. Chrome keeps rejecting them, so nothing here
/// notices when the library stops sending one: whoever fixes it deletes the
/// row. The scope is hand-kept, like everywhere in these tables.
const KNOWN_MISSING: &[(&str, Scope, &str)] = &[];

/// Commands in [`COMMANDS`] that aren't sent: they take no parameters to
/// mistype, so they would run, and running them would end the test.
const NOT_SENT: &[&str] = &["Browser.close"];

/// Every CDP event the library listens for.
const EVENTS: &[&str] = &[
    "Browser.downloadProgress",
    "Fetch.authRequired",
    "Fetch.requestPaused",
    "Network.dataReceived",
    "Network.eventSourceMessageReceived",
    "Network.loadingFailed",
    "Network.loadingFinished",
    "Network.requestWillBeSent",
    "Network.responseReceived",
    "Network.webSocketClosed",
    "Network.webSocketCreated",
    "Network.webSocketFrameReceived",
    "Network.webSocketFrameSent",
    "Page.downloadWillBegin",
    "Page.fileChooserOpened",
    "Page.frameAttached",
    "Page.frameDetached",
    "Page.frameNavigated",
    "Page.frameStartedLoading",
    "Page.frameStoppedLoading",
    "Page.javascriptDialogOpening",
    "Page.loadEventFired",
    "Page.navigatedWithinDocument",
    "Target.attachedToTarget",
    "Target.detachedFromTarget",
];

/// How long one probing call may take. Chrome answers a parameter error at
/// once; this only bounds a wedged page.
const CALL_BUDGET: Duration = Duration::from_secs(5);

/// GET `path` from Chrome's DevTools HTTP endpoint and parse the body. Chrome
/// doesn't answer HTTP/1.0 and keeps HTTP/1.1 connections open, so this
/// reads exactly `Content-Length` bytes. Bounded by [`CALL_BUDGET`].
async fn http_get_json(host_port: &str, path: &str) -> Value {
    let get = async {
        let mut stream = tokio::net::TcpStream::connect(host_port)
            .await
            .expect("connect to DevTools HTTP");
        let request = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("send");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let n = stream.read(&mut chunk).await.expect("read");
            assert!(n > 0, "DevTools HTTP closed before the whole body arrived");
            buf.extend_from_slice(&chunk[..n]);
            let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or_else(|| panic!("no Content-Length in {head:?}"));
            let body = head_end + 4;
            if buf.len() >= body + length {
                return serde_json::from_slice(&buf[body..body + length]).expect("JSON body");
            }
        }
    };
    tokio::time::timeout(CALL_BUDGET, get)
        .await
        .unwrap_or_else(|_| panic!("GET {path} from {host_port} took over {CALL_BUDGET:?}"))
}

/// The protocol a running Chrome speaks, from its `/json/protocol`.
struct Protocol {
    domains: HashSet<String>,
    /// `Domain.command` to its schema entry.
    commands: HashMap<String, Value>,
    events: HashSet<String>,
    /// `Domain.Type` to its JSON type (`string`, `object`, ...).
    types: HashMap<String, String>,
}

impl Protocol {
    async fn of(tab: &Tab) -> Self {
        let inspector = tab.inspector_url().expect("inspector url");
        let host_port = inspector
            .strip_prefix("http://")
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_else(|| panic!("unexpected inspector url {inspector}"));
        let schema = http_get_json(host_port, "/json/protocol").await;

        let mut protocol = Self {
            domains: HashSet::new(),
            commands: HashMap::new(),
            events: HashSet::new(),
            types: HashMap::new(),
        };
        let domains = schema["domains"].as_array().expect("protocol domains");
        for domain in domains {
            let name = domain["domain"].as_str().expect("domain name");
            protocol.domains.insert(name.to_string());
            let entries = |key: &str| domain[key].as_array().cloned().unwrap_or_default();
            for command in entries("commands") {
                let id = format!("{name}.{}", command["name"].as_str().expect("command name"));
                protocol.commands.insert(id, command);
            }
            for event in entries("events") {
                let id = format!("{name}.{}", event["name"].as_str().expect("event name"));
                protocol.events.insert(id);
            }
            for ty in entries("types") {
                let id = format!("{name}.{}", ty["id"].as_str().expect("type id"));
                let json_type = ty["type"].as_str().expect("type's type").to_string();
                protocol.types.insert(id, json_type);
            }
        }
        protocol
    }

    /// Parameters for `command` that Chrome rejects before running it: its
    /// first parameter that has a definite type, given a value of another
    /// type. `None` if it has no such parameter.
    fn mistyped_params(&self, command: &str) -> Option<Value> {
        let domain = command.split('.').next().expect("Domain.command");
        let params = self.commands[command]["parameters"].as_array()?;
        params.iter().find_map(|param| {
            let json_type = match (param["type"].as_str(), param["$ref"].as_str()) {
                (Some(t), _) => t.to_string(),
                (None, Some(r)) => {
                    let id = if r.contains('.') {
                        r.to_string()
                    } else {
                        format!("{domain}.{r}")
                    };
                    self.types.get(&id)?.clone()
                }
                (None, None) => return None,
            };
            let wrong = match json_type.as_str() {
                "string" => json!(0),
                "integer" | "number" | "boolean" | "array" | "object" => json!("x"),
                _ => return None,
            };
            let name = param["name"].as_str().expect("parameter name");
            Some(json!({ name: wrong }))
        })
    }
}

/// Send `command` with `params` at `scope`.
async fn send(
    browser: &TestBrowser,
    tab: &Tab,
    command: &str,
    scope: Scope,
    params: Value,
) -> Result<Value, CallError> {
    let budget = Some(CALL_BUDGET);
    match scope {
        Scope::Browser => {
            browser
                .cdp()
                .call_raw_with_timeout(command, params, None, budget)
                .await
        }
        Scope::Page => {
            let session = tab.session();
            let session_id = session.session_id().map(str::to_string);
            session
                .connection()
                .call_raw_with_timeout(command, params, session_id, budget)
                .await
        }
    }
}

/// A browser whose main tab is on a fixture page, and the server behind
/// it, which the caller must keep alive.
async fn launch_on_a_page() -> (FixtureServer, TestBrowser) {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    browser
        .main_tab()
        .goto(server.url(SITE_A, "/plain"))
        .await
        .expect("goto");
    (server, browser)
}

#[tokio::test]
async fn every_command_exists_where_the_library_sends_it() {
    let (_server, browser) = launch_on_a_page().await;
    let tab = browser.main_tab();
    let protocol = Protocol::of(&tab).await;

    let known: Vec<(&str, Scope, Option<&str>)> = COMMANDS
        .iter()
        .map(|&(c, s)| (c, s, None))
        .chain(KNOWN_MISSING.iter().map(|&(c, s, why)| (c, s, Some(why))))
        .collect();
    let mut problems = Vec::new();
    for (command, scope, known_bug) in known {
        if !protocol.commands.contains_key(command) {
            problems.push(format!("{command}: not in Chrome's protocol"));
            continue;
        }
        if NOT_SENT.contains(&command) {
            continue;
        }
        let mistyped = protocol.mistyped_params(command);
        let sent_as_is = mistyped.is_none();
        let params = mistyped.unwrap_or_else(|| json!({}));
        let at = format!("{command} at {scope:?}");
        match (
            known_bug,
            send(&browser, &tab, command, scope, params).await,
        ) {
            (None, Err(CallError::Rpc(-32601, ..))) => problems.push(format!("{at}: not found")),
            (None, Err(CallError::Rpc(..))) => {}
            (None, Ok(_)) if sent_as_is => {}
            (None, Ok(_)) => problems.push(format!("{at}: ran despite a mistyped parameter")),
            (Some(why), Err(CallError::Rpc(-32601, ..))) => {
                eprintln!("known missing, as expected: {at} ({why})");
            }
            (Some(why), reply) => problems.push(format!(
                "{at} is in KNOWN_MISSING ({why}) but got {reply:?}; if it's fixed, delete \
                 the row"
            )),
            (None, Err(e)) => problems.push(format!("{at}: {e}")),
        }
    }
    assert!(
        problems.is_empty(),
        "CDP contract broken:\n  {}",
        problems.join("\n  ")
    );

    browser.close().await;
}

/// The workspace's `crates/*/src` files, minus the test kit's (its CDP calls
/// aren't the library's).
fn library_sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("read crates/") {
        let krate = entry.expect("crate dir").path();
        if krate.ends_with("zendriver-testkit") || !krate.join("src").is_dir() {
            continue;
        }
        walk(&krate.join("src"), &mut files);
    }
    assert!(
        files.len() > 50,
        "found only {} source files under {crates:?}",
        files.len()
    );
    files
}

/// Every quoted `"Domain.member"` in the library's source, outside comments,
/// whose `Domain` is a CDP domain.
fn cdp_names_in_source(domains: &HashSet<String>) -> BTreeSet<String> {
    let quoted = Regex::new(r#""([A-Z][A-Za-z]+)\.([a-z][A-Za-z]+)""#).expect("regex");
    let mut names = BTreeSet::new();
    for file in library_sources() {
        let text = std::fs::read_to_string(&file).expect("read source");
        for line in text.lines().filter(|l| !l.trim_start().starts_with("//")) {
            for name in quoted.captures_iter(line) {
                if domains.contains(&name[1]) {
                    names.insert(format!("{}.{}", &name[1], &name[2]));
                }
            }
        }
    }
    names
}

#[tokio::test]
async fn every_event_exists_and_the_tables_match_the_source() {
    let (_server, browser) = launch_on_a_page().await;
    let protocol = Protocol::of(&browser.main_tab()).await;

    let missing_events: Vec<_> = EVENTS
        .iter()
        .filter(|e| !protocol.events.contains(**e))
        .collect();
    assert!(
        missing_events.is_empty(),
        "events Chrome doesn't have: {missing_events:?}"
    );

    let tabled: BTreeSet<String> = COMMANDS
        .iter()
        .map(|(c, _)| *c)
        .chain(KNOWN_MISSING.iter().map(|(c, ..)| *c))
        .chain(EVENTS.iter().copied())
        .map(str::to_string)
        .collect();
    let in_source = cdp_names_in_source(&protocol.domains);
    let untabled: Vec<_> = in_source.difference(&tabled).collect();
    let stale: Vec<_> = tabled.difference(&in_source).collect();
    assert!(
        untabled.is_empty(),
        "CDP names in the source without a row in COMMANDS or EVENTS: {untabled:?}"
    );
    assert!(
        stale.is_empty(),
        "rows whose name no longer appears in the source: {stale:?}"
    );

    browser.close().await;
}
