//! Drive the real `zendriver-mcp` binary over stdio or HTTP, as an MCP client
//! does, and stop it the ways a client or the OS might.

// Each test binary that includes this module uses only some of it.
#![allow(dead_code)]

use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::oneshot;
use zendriver_testkit::ChromeTree;

const BIN_PATH: &str = env!("CARGO_BIN_EXE_zendriver-mcp");

/// How long the HTTP server gets to log the address it's listening on.
const LISTENING_WITHIN: Duration = Duration::from_secs(10);

/// What the HTTP server logs once it's listening; the line also has the
/// bound address.
const LISTENING_LINE: &str = "zendriver-mcp HTTP listening";

#[derive(Clone, Copy, Debug)]
pub enum Transport {
    Stdio,
    Http,
}

/// A running `zendriver-mcp` with a connected client. Dropping it kills the
/// server, and the Chrome it launched, if any.
pub struct McpServer {
    child: Child,
    /// `None` once [`McpServer::close_stdin`] has disconnected it.
    client: Option<RunningService<RoleClient, ()>>,
    /// Everything the server has logged so far, one entry per line.
    stderr: Arc<Mutex<Vec<String>>>,
    /// The Chrome the last [`McpServer::open`] launched.
    chrome: Option<ChromeTree>,
}

/// The `127.0.0.1:PORT` in a log line, which may contain ANSI colour codes.
fn loopback_address(line: &str) -> Option<String> {
    let at = line.find("127.0.0.1:")?;
    let rest = &line[at + "127.0.0.1:".len()..];
    let port: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (!port.is_empty()).then(|| format!("127.0.0.1:{port}"))
}

/// Read `stderr` line by line into `lines` until it closes, so the server
/// never blocks on a full pipe. Sends the bound address to `listening` when
/// the HTTP server logs it.
async fn drain(
    stderr: ChildStderr,
    lines: Arc<Mutex<Vec<String>>>,
    listening: oneshot::Sender<String>,
) {
    let mut listening = Some(listening);
    let mut reader = BufReader::new(stderr);
    let mut bytes = Vec::new();
    while reader
        .read_until(b'\n', &mut bytes)
        .await
        .is_ok_and(|n| n > 0)
    {
        let line = String::from_utf8_lossy(&bytes).trim_end().to_string();
        bytes.clear();
        if line.contains(LISTENING_LINE) {
            if let Some(addr) = loopback_address(&line) {
                if let Some(tx) = listening.take() {
                    let _ = tx.send(addr);
                }
            }
        }
        lines.lock().expect("stderr lines").push(line);
    }
}

/// The JSON a tool call returned.
fn structured(result: rmcp::model::CallToolResult) -> Value {
    if let Some(value) = result.structured_content {
        return value;
    }
    let text = result
        .content
        .iter()
        .find_map(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("a tool result has structured or text content");
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

impl McpServer {
    /// Start the server on `transport` (HTTP on a free loopback port) and
    /// connect a client to it.
    pub async fn start(transport: Transport) -> Self {
        let mut command = Command::new(BIN_PATH);
        // Killed if `start` fails before there's an `McpServer` to drop.
        command
            .args(["--log", "info"])
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match transport {
            Transport::Stdio => command.stdin(Stdio::piped()).stdout(Stdio::piped()),
            Transport::Http => command
                .args(["--http", "127.0.0.1:0"])
                .stdin(Stdio::null())
                .stdout(Stdio::null()),
        };
        let mut child = command.spawn().expect("spawn zendriver-mcp");
        let stderr = Arc::<Mutex<Vec<String>>>::default();
        let (listening_tx, listening_rx) = oneshot::channel();
        tokio::spawn(drain(
            child.stderr.take().expect("piped stderr"),
            stderr.clone(),
            listening_tx,
        ));

        let client = match transport {
            Transport::Stdio => {
                let stdout = child.stdout.take().expect("piped stdout");
                let stdin = child.stdin.take().expect("piped stdin");
                ().serve((stdout, stdin))
                    .await
                    .expect("MCP client over stdio")
            }
            Transport::Http => {
                let addr = tokio::time::timeout(LISTENING_WITHIN, listening_rx)
                    .await
                    .expect("the HTTP server didn't say where it's listening")
                    .expect("the server exited before it was listening");
                // reqwest (inside rmcp's client) needs a TLS provider even
                // for plain HTTP.
                let _ = rustls::crypto::ring::default_provider().install_default();
                let uri = format!("http://{addr}/mcp");
                let transport = StreamableHttpClientTransport::from_uri(uri.as_str());
                ().serve(transport).await.expect("MCP client over HTTP")
            }
        };
        Self {
            child,
            client: Some(client),
            stderr,
            chrome: None,
        }
    }

    /// What the server has logged so far.
    pub fn stderr(&self) -> String {
        self.stderr.lock().expect("stderr lines").join("\n")
    }

    /// Call `tool` with `arguments` (a JSON object). Returns its result, or
    /// the error the call or the tool reported.
    pub async fn try_call(&self, tool: &str, arguments: Value) -> Result<Value, String> {
        let Value::Object(arguments) = arguments else {
            panic!("{tool}: arguments must be a JSON object, not {arguments}");
        };
        let client = self.client.as_ref().expect("client disconnected");
        let params = CallToolRequestParams::new(tool.to_string()).with_arguments(arguments);
        let result = client
            .call_tool(params)
            .await
            .map_err(|e| format!("{tool}: {e}"))?;
        if result.is_error == Some(true) {
            return Err(format!("{tool} reported an error: {:?}", result.content));
        }
        Ok(structured(result))
    }

    /// [`McpServer::try_call`], panicking on an error.
    pub async fn call(&self, tool: &str, arguments: Value) -> Value {
        self.try_call(tool, arguments)
            .await
            .unwrap_or_else(|e| panic!("{e}\nserver log:\n{}", self.stderr()))
    }

    /// `browser_open` with `arguments`, then note which Chrome it launched,
    /// for [`McpServer::chrome`] and so that dropping this kills it.
    pub async fn open(&mut self, arguments: Value) -> Value {
        let opened = self.call("browser_open", arguments).await;
        let pid = self.child.id().expect("the server is running");
        self.chrome = Some(ChromeTree::launched_by(pid).await);
        opened
    }

    /// The Chrome the last [`McpServer::open`] launched.
    pub fn chrome(&self) -> ChromeTree {
        self.chrome.clone().expect("no McpServer::open yet")
    }

    /// Disconnect the client, which for stdio closes the server's stdin, as
    /// a client does when it's done.
    pub async fn close_stdin(&mut self) {
        let client = self.client.take().expect("client already disconnected");
        let _ = client.cancel().await;
    }

    /// Send the server `signal`, e.g. `"TERM"` or `"KILL"`.
    pub fn signal(&self, signal: &str) {
        let pid = self.child.id().expect("the server is running");
        let status = std::process::Command::new("kill")
            .args(["-s", signal, &pid.to_string()])
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -s {signal} {pid} failed");
    }

    /// Wait up to `within` for the server to exit; `None` if it hasn't.
    pub async fn wait_exit(&mut self, within: Duration) -> Option<ExitStatus> {
        let exited = tokio::time::timeout(within, self.child.wait()).await.ok()?;
        Some(exited.expect("wait for the server"))
    }
}

impl Drop for McpServer {
    /// Kill the server, then its Chrome: a test that fails before closing
    /// the browser would otherwise leave Chrome running, since Chrome
    /// outlives a killed server.
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        if let Some(chrome) = &self.chrome {
            chrome.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_address_among_colour_codes() {
        let line = "\u{1b}[2m2026\u{1b}[0m INFO zendriver-mcp HTTP listening on /mcp \
                    \u{1b}[3maddr\u{1b}[0m\u{1b}[2m=\u{1b}[0m127.0.0.1:41234";
        assert_eq!(loopback_address(line).as_deref(), Some("127.0.0.1:41234"));
        assert_eq!(loopback_address("no address here"), None);
    }
}
