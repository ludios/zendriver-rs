//! A local HTTP server with canned pages, and a log of every request it saw.
//!
//! One server answers for every host: Chrome launched by
//! [`launch_for_test`](crate::launch_for_test) resolves every `*.test` name to
//! 127.0.0.1, so `http://a.test:PORT/` and `http://b.test:PORT/` reach the
//! same listener as two different sites. Pages that care which site they are
//! on read the `Host` header.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// A test site. Cross-site from [`SITE_B`].
pub const SITE_A: &str = "a.test";

/// A second test site, cross-site from [`SITE_A`].
pub const SITE_B: &str = "b.test";

/// Chrome flag that resolves every `*.test` host to the fixture server's
/// address. [`launch_for_test`](crate::launch_for_test) adds it.
pub const HOST_RESOLVER_RULES: &str = "--host-resolver-rules=MAP *.test 127.0.0.1";

/// Body served by `/download`.
pub const DOWNLOAD_BODY: &str = "id,name\n1,alpha\n2,beta\n";

/// Username `/basic-auth` accepts.
pub const BASIC_AUTH_USER: &str = "fixture-user";

/// Password `/basic-auth` accepts.
pub const BASIC_AUTH_PASSWORD: &str = "fixture-password";

/// A 1x1 transparent GIF, served as the slow subresource.
const PIXEL_GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

/// One request the server received.
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    /// The `Host` header without its port, e.g. `a.test`; empty if absent.
    pub host: String,
    pub method: Method,
    /// Path and query, e.g. `/form-result?q=x`.
    pub uri: Uri,
    pub headers: HeaderMap,
    /// When the request reached the server.
    pub received_at: Instant,
    /// When the handler produced the response head; `None` while in flight.
    /// A streamed body (`/trickle`) may still be arriving after this.
    pub responded_at: Option<Instant>,
}

impl RecordedRequest {
    /// The value of header `name`, if present and valid UTF-8.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

type RequestLog = Arc<Mutex<Vec<RecordedRequest>>>;

/// The body `/trickle?chunks={chunks}&chunk_bytes={chunk_bytes}` sends:
/// chunk `i` is `chunk_bytes` copies of the letter `a + i % 26`, so a
/// truncated or reordered body doesn't compare equal.
pub fn trickle_body(chunks: usize, chunk_bytes: usize) -> Vec<u8> {
    (0..chunks)
        .flat_map(|i| trickle_chunk(i, chunk_bytes))
        .collect()
}

fn trickle_chunk(index: usize, chunk_bytes: usize) -> Vec<u8> {
    let letter = b'a' + u8::try_from(index % 26).expect("index % 26 fits in u8");
    vec![letter; chunk_bytes]
}

type Params = Query<HashMap<String, String>>;

/// Numeric query parameter `name`, or `default` when absent. A value that
/// isn't a number is a bug in the test, so it panics the handler (and the
/// request fails) rather than quietly using the default.
fn number(params: &Params, name: &str, default: u64) -> u64 {
    params.get(name).map_or(default, |v| {
        v.parse()
            .unwrap_or_else(|_| panic!("query parameter {name}={v:?} isn't a number"))
    })
}

/// The `Host` header split into host and port; empty and `None` if absent.
fn host_and_port(headers: &HeaderMap) -> (String, Option<u16>) {
    let value = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    match value.rsplit_once(':') {
        Some((host, port)) => (host.to_string(), port.parse().ok()),
        None => (value.to_string(), None),
    }
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A standards-mode page. `title` is escaped; `head` and `body` are HTML.
fn page(title: &str, head: &str, body: &str) -> Html<String> {
    let title = escape_html(title);
    Html(format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>{title}</title>{head}</head>
<body>
{body}
</body></html>
"#
    ))
}

async fn plain(headers: HeaderMap, params: Params) -> Html<String> {
    let title = params.get("title").map_or("Plain", String::as_str);
    let heading = escape_html(title);
    let host = escape_html(&host_and_port(&headers).0);
    page(
        title,
        "",
        &format!(
            r#"<h1 id="heading">{heading}</h1>
<p id="host">{host}</p>
<a id="link" href="/plain?title=Linked">Linked page</a>"#
        ),
    )
}

async fn slow_load(params: Params) -> Html<String> {
    let delay_ms = number(&params, "delay_ms", 1000);
    let iframe = if number(&params, "iframe", 0) != 0 {
        r#"<iframe id="child" name="child" src="/frame-child"></iframe>"#
    } else {
        ""
    };
    page(
        "Slow load",
        "",
        &format!(
            r#"<h1 id="heading">Slow load</h1>
{iframe}
<img id="slow" src="/slow-resource?delay_ms={delay_ms}" width="1" height="1">"#
        ),
    )
}

async fn slow_resource(params: Params) -> impl IntoResponse {
    tokio::time::sleep(Duration::from_millis(number(&params, "delay_ms", 1000))).await;
    (
        [
            (header::CONTENT_TYPE, "image/gif"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        PIXEL_GIF,
    )
}

async fn form() -> Html<String> {
    page(
        "Form",
        "",
        r#"<form id="form" action="/form-result" method="get">
<input id="q" name="q" autofocus>
<button id="submit" type="submit">Search</button>
</form>"#,
    )
}

async fn form_result(params: Params) -> Html<String> {
    let q = escape_html(params.get("q").map_or("", String::as_str));
    page(
        "Form result",
        "",
        &format!(
            r#"<h1 id="heading">Form result</h1>
<p id="q">{q}</p>"#
        ),
    )
}

/// One of `/redirect/N`'s 302s. `N` must be at least 1.
async fn redirect(Path(remaining): Path<u32>) -> Response {
    let next = match remaining {
        0 => return (StatusCode::BAD_REQUEST, "/redirect/N needs N >= 1").into_response(),
        1 => "/plain?title=Redirected".to_string(),
        n => format!("/redirect/{}", n - 1),
    };
    (StatusCode::FOUND, [(header::LOCATION, next)]).into_response()
}

async fn iframe_same_origin() -> Html<String> {
    page(
        "Same-origin iframe",
        "",
        r#"<h1 id="heading">Parent</h1>
<iframe id="child" name="child" src="/frame-child"></iframe>"#,
    )
}

async fn iframe_cross_site(headers: HeaderMap) -> Result<Html<String>, (StatusCode, &'static str)> {
    let (host, port) = host_and_port(&headers);
    let port = port.ok_or((StatusCode::BAD_REQUEST, "Host header has no port"))?;
    let other = if host == SITE_B { SITE_A } else { SITE_B };
    Ok(page(
        "Cross-site iframe",
        "",
        &format!(
            r#"<h1 id="heading">Parent</h1>
<iframe id="child" name="child" src="http://{other}:{port}/frame-child"></iframe>"#
        ),
    ))
}

async fn frame_child(headers: HeaderMap) -> Html<String> {
    let host = escape_html(&host_and_port(&headers).0);
    page(
        "Frame child",
        "",
        &format!(
            r#"<p id="child-host">{host}</p>
<button id="child-button" onclick="window.childClicked = true">Child button</button>"#
        ),
    )
}

async fn downloads() -> Html<String> {
    page(
        "Downloads",
        "",
        r#"<a id="download" href="/download">report.csv</a>
<a id="download-tiny" href="/download-tiny">tiny.txt</a>"#,
    )
}

/// An attachment response named `filename`.
fn attachment(filename: &str, content_type: &'static str, body: &'static str) -> Response {
    let disposition = HeaderValue::from_str(&format!(r#"attachment; filename="{filename}""#))
        .expect("valid Content-Disposition");
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        body,
    )
        .into_response()
}

async fn download() -> Response {
    attachment("report.csv", "text/csv", DOWNLOAD_BODY)
}

async fn download_tiny() -> Response {
    attachment("tiny.txt", "text/plain", "x")
}

async fn basic_auth(headers: HeaderMap) -> Response {
    let expected = format!(
        "Basic {}",
        BASE64.encode(format!("{BASIC_AUTH_USER}:{BASIC_AUTH_PASSWORD}"))
    );
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if given == Some(expected.as_str()) {
        page("Authorized", "", r#"<h1 id="heading">Authorized</h1>"#).into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, r#"Basic realm="fixture""#)],
            page("Unauthorized", "", r#"<h1 id="heading">Unauthorized</h1>"#),
        )
            .into_response()
    }
}

async fn trickle(params: Params) -> Response {
    let chunks = usize::try_from(number(&params, "chunks", 8)).expect("chunks fits in usize");
    let chunk_bytes =
        usize::try_from(number(&params, "chunk_bytes", 1024)).expect("chunk_bytes fits in usize");
    let interval = Duration::from_millis(number(&params, "interval_ms", 100));
    let stream = futures::stream::unfold(0, move |index| async move {
        if index == chunks {
            return None;
        }
        if index > 0 && !interval.is_zero() {
            tokio::time::sleep(interval).await;
        }
        Some((
            Ok::<_, Infallible>(trickle_chunk(index, chunk_bytes)),
            index + 1,
        ))
    });
    (
        [(header::CONTENT_TYPE, "text/plain")],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn svg_sprite() -> Html<String> {
    page(
        "SVG sprite",
        "",
        r##"<svg width="0" height="0" style="position: absolute">
  <symbol id="icon" viewBox="0 0 10 10"><circle cx="5" cy="5" r="4"/></symbol>
</svg>
<svg id="use-href" width="20" height="20"><use href="#icon"/></svg>
<svg id="use-xlink" width="20" height="20"><use xlink:href="#icon"/></svg>
<svg id="use-external" width="20" height="20"><use href="/sprite.svg#icon"/></svg>
<a id="link" href="/plain">Plain</a>"##,
    )
}

async fn sprite_svg() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        r#"<svg xmlns="http://www.w3.org/2000/svg">
  <symbol id="icon" viewBox="0 0 10 10"><rect width="10" height="10"/></symbol>
</svg>"#,
    )
}

async fn below_fold() -> Html<String> {
    page(
        "Below the fold",
        "",
        r#"<div style="height: 3000px">Spacer</div>
<button id="below" onclick="window.belowClicked = true">Below the fold</button>"#,
    )
}

/// Padding and border dwarf the content box and differ per axis, so mixing
/// up two of the element's boxes moves its computed centre by a measurable,
/// non-square offset.
async fn padded_button() -> Html<String> {
    page(
        "Padded button",
        "<script>window.clicks = [];</script>",
        r#"<button id="padded"
  style="margin: 40px; padding: 20px 60px; border: 25px solid #345; font-size: 16px"
  onclick="window.clicks.push([event.clientX, event.clientY])">Padded</button>"#,
    )
}

async fn controls() -> Html<String> {
    page(
        "Controls",
        "",
        r#"<select id="fruit" name="fruit">
  <option value="apple">Apple</option>
  <option value="banana" selected>Banana</option>
  <option value="cherry">Cherry</option>
</select>
<textarea id="notes">Some notes</textarea>
<div id="editable" contenteditable="true">Editable text</div>"#,
    )
}

async fn quirks() -> Html<&'static str> {
    Html(
        r#"<html><head><title>Quirks</title></head>
<body><p id="mode">No doctype</p></body></html>
"#,
    )
}

/// The cell is far wider than its link, so a click at the cell's centre
/// misses the link.
async fn table_link() -> Html<String> {
    page(
        "Files",
        "<style>td.name { width: 400px; text-align: left; }</style>",
        r#"<table id="files">
  <tr>
    <td class="icon">F</td>
    <td class="name"><a id="readme-link" href="/plain?title=README">README.md</a></td>
    <td class="message">Initial commit</td>
  </tr>
  <tr>
    <td class="icon">F</td>
    <td class="name"><a id="license-link" href="/plain?title=LICENSE">LICENSE</a></td>
    <td class="message">Add license</td>
  </tr>
</table>"#,
    )
}

/// Middleware: log the request on arrival, then note when it was answered.
async fn record(State(log): State<RequestLog>, request: Request, next: Next) -> Response {
    let index = {
        let mut log = log.lock().expect("request log");
        log.push(RecordedRequest {
            host: host_and_port(request.headers()).0,
            method: request.method().clone(),
            uri: request.uri().clone(),
            headers: request.headers().clone(),
            received_at: Instant::now(),
            responded_at: None,
        });
        log.len() - 1
    };
    let response = next.run(request).await;
    log.lock().expect("request log")[index].responded_at = Some(Instant::now());
    response
}

fn canned_routes() -> Router {
    Router::new()
        .route("/plain", get(plain))
        .route("/slow-load", get(slow_load))
        .route("/slow-resource", get(slow_resource))
        .route("/form", get(form))
        .route("/form-result", get(form_result))
        .route("/redirect/:remaining", get(redirect))
        .route("/iframe-same-origin", get(iframe_same_origin))
        .route("/iframe-cross-site", get(iframe_cross_site))
        .route("/frame-child", get(frame_child))
        .route("/downloads", get(downloads))
        .route("/download", get(download))
        .route("/download-tiny", get(download_tiny))
        .route("/basic-auth", get(basic_auth))
        .route("/trickle", get(trickle))
        .route("/svg-sprite", get(svg_sprite))
        .route("/sprite.svg", get(sprite_svg))
        .route("/below-fold", get(below_fold))
        .route("/padded-button", get(padded_button))
        .route("/controls", get(controls))
        .route("/quirks", get(quirks))
        .route("/table-link", get(table_link))
}

/// The fixture server. Dropping it stops it, closing idle keep-alive
/// connections too; requests in flight are answered first.
///
/// Canned routes (`delay_ms` and the other numbers are query parameters):
///
/// | Route | What it is |
/// |-------|------------|
/// | `/plain?title=T` | A page titled `T` (default `Plain`) with `#heading`, `#host`, and a `#link` to `/plain?title=Linked` |
/// | `/slow-load?delay_ms=N&iframe=1` | Its `#slow` image takes `N` ms (default 1000), so `load` fires late; `iframe=1` adds a fast `#child` iframe that finishes first |
/// | `/slow-resource?delay_ms=N` | The image behind `/slow-load`: a GIF sent after `N` ms, `no-store` |
/// | `/form` | `#form` with input `#q` that GETs `/form-result` on submit (Enter or `#submit`) |
/// | `/form-result?q=V` | Titled `Form result`; shows `V` in `#q` |
/// | `/redirect/N` | `N` (at least 1) 302s, then `/plain?title=Redirected` |
/// | `/iframe-same-origin` | `#child` iframe of `/frame-child` on the same origin |
/// | `/iframe-cross-site` | `#child` iframe of `/frame-child` on the other test site |
/// | `/frame-child` | `#child-host` showing its host, and `#child-button` |
/// | `/downloads` | Links `#download` and `#download-tiny` to the next two |
/// | `/download` | `report.csv` attachment, body [`DOWNLOAD_BODY`] |
/// | `/download-tiny` | `tiny.txt` attachment of one byte |
/// | `/basic-auth` | `401` with a `Basic` challenge unless given [`BASIC_AUTH_USER`]:[`BASIC_AUTH_PASSWORD`] |
/// | `/trickle?chunks=N&chunk_bytes=K&interval_ms=M` | [`trickle_body`]`(N, K)`, one chunk every `M` ms (defaults 8, 1024, 100); `interval_ms=0` makes it a plain large body |
/// | `/svg-sprite` | `<use>` of an inline symbol (`href` and `xlink:href`) and of `/sprite.svg` |
/// | `/below-fold` | `#below` button 3000 px down; clicking sets `window.belowClicked` |
/// | `/padded-button` | `#padded` button with large, asymmetric padding and border; clicks push `[clientX, clientY]` onto `window.clicks` |
/// | `/controls` | `<select id="fruit">`, `<textarea id="notes">`, contenteditable `#editable` |
/// | `/quirks` | No doctype, so `document.compatMode` is `BackCompat` |
/// | `/table-link` | GitHub-style file table; the wide `td.name` cell's text is exactly its link's (`#readme-link`, `README.md`) |
pub struct FixtureServer {
    port: u16,
    log: RequestLog,
    /// Dropped (sending nothing) to shut the server down.
    _stop: oneshot::Sender<()>,
}

impl FixtureServer {
    /// Start a server with only the canned routes.
    pub async fn start() -> Self {
        Self::start_with(Router::new()).await
    }

    /// Start a server with the canned routes plus `extra`. Requests to
    /// `extra` are logged like any other.
    ///
    /// Panics if `extra` has a route that's also canned.
    pub async fn start_with(extra: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let port = listener.local_addr().expect("local addr").port();
        let log = RequestLog::default();
        let app = canned_routes()
            .merge(extra)
            .layer(middleware::from_fn_with_state(log.clone(), record));
        let (stop, stopped) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .expect("fixture server");
        });
        Self {
            port,
            log,
            _stop: stop,
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `http://{host}:{port}{path}`. `host` should be [`SITE_A`], [`SITE_B`],
    /// another `*.test` name, or `127.0.0.1`; `path` starts with `/`.
    pub fn url(&self, host: &str, path: &str) -> String {
        assert!(path.starts_with('/'), "path must start with '/': {path:?}");
        format!("http://{host}:{}{path}", self.port)
    }

    /// Every request so far, oldest first.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.log.lock().expect("request log").clone()
    }

    /// Requests whose path (ignoring the query) is `path`, oldest first.
    pub fn requests_to(&self, path: &str) -> Vec<RecordedRequest> {
        let mut requests = self.requests();
        requests.retain(|r| r.uri.path() == path);
        requests
    }

    /// When the latest response to `path` was produced, or `None` if no
    /// request to it has been answered yet.
    pub fn last_responded_at(&self, path: &str) -> Option<Instant> {
        self.requests_to(path)
            .iter()
            .rev()
            .find_map(|r| r.responded_at)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::*;

    async fn connect(server: &FixtureServer) -> TcpStream {
        TcpStream::connect(("127.0.0.1", server.port()))
            .await
            .expect("connect")
    }

    /// `GET path` on a keep-alive `stream`; returns the lowercased response
    /// head and the body.
    async fn get(stream: &mut TcpStream, path: &str) -> (String, String) {
        let request = format!("GET {path} HTTP/1.1\r\nHost: {SITE_A}\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("send");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).await.expect("read");
            assert!(n > 0, "connection closed mid-response");
            buf.extend_from_slice(&chunk[..n]);
            let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map_or(0, |v| v.trim().parse().expect("Content-Length"));
            if buf.len() >= end + 4 + length {
                let body = String::from_utf8_lossy(&buf[end + 4..end + 4 + length]);
                return (head, body.into_owned());
            }
        }
    }

    #[tokio::test]
    async fn redirect_n_is_n_302s() {
        let server = FixtureServer::start().await;
        let mut stream = connect(&server).await;
        let (head, _) = get(&mut stream, "/redirect/2").await;
        assert!(head.starts_with("http/1.1 302"), "{head}");
        assert!(head.contains("location: /redirect/1"), "{head}");
        let (head, _) = get(&mut stream, "/redirect/1").await;
        assert!(head.contains("location: /plain?title=redirected"), "{head}");
    }

    #[tokio::test]
    async fn slow_load_has_an_iframe_only_when_asked() {
        let server = FixtureServer::start().await;
        let mut stream = connect(&server).await;
        let (_, body) = get(&mut stream, "/slow-load?delay_ms=0&iframe=0").await;
        assert!(!body.contains("<iframe"), "{body}");
        let (_, body) = get(&mut stream, "/slow-load?delay_ms=0&iframe=1").await;
        assert!(body.contains("<iframe"), "{body}");
    }

    #[tokio::test]
    async fn dropping_the_server_closes_keep_alive_connections() {
        let server = FixtureServer::start().await;
        let mut stream = connect(&server).await;
        get(&mut stream, "/plain").await;
        drop(server);
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
            .await
            .expect("the idle connection should be closed, not left serving");
        assert_eq!(read.unwrap_or(0), 0, "expected EOF");
    }
}
