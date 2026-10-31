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
/// server (not the Chrome it launched).
pub struct McpServer {
    child: Child,
    /// `None` once [`McpServer::close_stdin`] has disconnected it.
    client: Option<RunningService<RoleClient, ()>>,
    /// Everything the server has logged so far, one entry per line.
    stderr: Arc<Mutex<Vec<String>>>,
}

/// The `127.0.0.1:PORT` in a log line, which may contain ANSI colour codes.
fn loopback_address(line: &str) -> Option<String> {
    let at = line.find("127.0.0.1:")?;
    let rest = &line[at + "127.0.0.1:".len()..];
    let port: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (!port.is_empty()).then(|| format!("127.0.0.1:{port}"))
}

/// Read `stderr` line by line into `lines` until it closes. Sends the bound
/// address to `listening` when the HTTP server logs it.
async fn drain(
    stderr: ChildStderr,
    lines: Arc<Mutex<Vec<String>>>,
    listening: oneshot::Sender<String>,
) {
    let mut listening = Some(listening);
    let mut reader = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        if line.contains(LISTENING_LINE) {
            if let (Some(tx), Some(addr)) = (listening.take(), loopback_address(&line)) {
                let _ = tx.send(addr);
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
        command
            .args(["--log", "info"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Transport::Http = transport {
            command.args(["--http", "127.0.0.1:0"]);
        }
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
        }
    }

    /// What the server has logged so far.
    pub fn stderr(&self) -> String {
        self.stderr.lock().expect("stderr lines").join("\n")
    }

    /// Call `tool` with `arguments` (a JSON object) and return its result.
    /// Panics if the call fails or the tool reports an error.
    pub async fn call(&self, tool: &str, arguments: Value) -> Value {
        let Value::Object(arguments) = arguments else {
            panic!("{tool}: arguments must be a JSON object, not {arguments}");
        };
        let client = self.client.as_ref().expect("client disconnected");
        let params = CallToolRequestParams::new(tool.to_string()).with_arguments(arguments);
        let result = client
            .call_tool(params)
            .await
            .unwrap_or_else(|e| panic!("{tool}: {e}\nserver log:\n{}", self.stderr()));
        if result.is_error == Some(true) {
            panic!("{tool} reported an error: {:?}", result.content);
        }
        structured(result)
    }

    /// The Chrome this server launched. Call it after `browser_open`.
    pub async fn chrome(&self) -> ChromeTree {
        ChromeTree::launched_by(self.child.id().expect("the server is running")).await
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
