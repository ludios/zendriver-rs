//! The stealth consistency probe: the browser's identity read from four
//! places, which should all tell the same story.
//!
//! [`PROBE_PATH`] is a page that reads the identity with one script in three
//! realms: its main world, a same-origin iframe, and a dedicated worker.
//! [`PROBE_JS`], evaluated in the page once it has loaded, returns the three
//! readings. The page's requests (the document, the iframe, the scripts the
//! worker loads) carry the identity the HTTP headers claim, and land in the
//! [`FixtureServer`](crate::FixtureServer)'s request log. [`disagreements`]
//! compares all of it.
//!
//! Serve the probe from `127.0.0.1`, not a `*.test` site:
//! `navigator.userAgentData` and the `Sec-CH-UA-*` headers exist only in
//! secure contexts, and plain HTTP is one only on loopback.

use std::collections::BTreeMap;

use axum::Router;
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use serde_json::{Value, json};

use crate::server::RecordedRequest;

/// The probe page. Every request it causes has a path starting with this.
pub const PROBE_PATH: &str = "/probe";

/// Evaluate in the loaded probe page (awaiting the promise) to read the
/// identity in each realm: `{"main": {...}, "iframe": {...}, "worker": {...}}`.
pub const PROBE_JS: &str = "zdProbe()";

/// The high-entropy client hints the probe page asks for, so that the
/// requests after it carry them too.
const ACCEPT_CH: &str = "Sec-CH-UA-Arch, Sec-CH-UA-Bitness, Sec-CH-UA-Full-Version-List, \
                         Sec-CH-UA-Model, Sec-CH-UA-Platform-Version, Sec-CH-UA-WoW64";

/// Defines `zdIdentity()`, which reads the identity of whatever realm it was
/// loaded into (a window or a worker).
const IDENTITY_JS: &str = r#"
function zdWebgl() {
  try {
    const canvas = typeof document === 'undefined'
      ? new OffscreenCanvas(1, 1)
      : document.createElement('canvas');
    const gl = canvas.getContext('webgl');
    if (!gl) return 'no webgl';
    const info = gl.getExtension('WEBGL_debug_renderer_info');
    if (!info) return 'no WEBGL_debug_renderer_info';
    return {
      vendor: gl.getParameter(info.UNMASKED_VENDOR_WEBGL),
      renderer: gl.getParameter(info.UNMASKED_RENDERER_WEBGL),
    };
  } catch (e) {
    return 'error: ' + e;
  }
}

async function zdIdentity() {
  const nav = navigator;
  const id = {
    userAgent: nav.userAgent,
    platform: nav.platform,
    hardwareConcurrency: nav.hardwareConcurrency,
    deviceMemory: nav.deviceMemory ?? null,
    languages: [...nav.languages],
    timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
    webgl: zdWebgl(),
    uaData: null,
    uaDataHigh: null,
  };
  const data = nav.userAgentData;
  if (data) {
    const brands = list => list.map(b => b.brand + '/' + b.version);
    id.uaData = { brands: brands(data.brands), mobile: data.mobile, platform: data.platform };
    try {
      const high = await data.getHighEntropyValues(
        ['architecture', 'bitness', 'fullVersionList', 'model', 'platformVersion', 'wow64']);
      id.uaDataHigh = {
        architecture: high.architecture,
        bitness: high.bitness,
        fullVersionList: brands(high.fullVersionList ?? []),
        model: high.model,
        platformVersion: high.platformVersion,
        wow64: high.wow64,
      };
    } catch (e) {
      id.uaDataHigh = 'error: ' + e;
    }
  }
  return id;
}
"#;

/// Starts the worker and gathers the three readings: `zdProbe()`.
const PAGE_JS: &str = r#"
async function zdProbe() {
  const worker = new Worker('/probe/worker.js');
  const fromWorker = new Promise((resolve, reject) => {
    worker.onmessage = event => resolve(event.data);
    worker.onerror = event => reject(new Error('probe worker failed: ' + event.message));
  });
  const frame = document.getElementById('frame').contentWindow;
  try {
    const [main, iframe, inWorker] = await Promise.all([zdIdentity(), frame.zdIdentity(), fromWorker]);
    return { main, iframe, worker: inWorker };
  } finally {
    worker.terminate();
  }
}
"#;

const WORKER_JS: &str = "importScripts('/probe/identity.js');\nzdIdentity().then(postMessage);\n";

fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>{title}</title>
<script src="/probe/identity.js"></script></head>
<body>
{body}
</body></html>
"#
    ))
}

async fn probe_page() -> impl IntoResponse {
    (
        [(header::HeaderName::from_static("accept-ch"), ACCEPT_CH)],
        page(
            "Probe",
            &format!(
                r#"<iframe id="frame" src="/probe/frame"></iframe>
<script>{PAGE_JS}</script>"#
            ),
        ),
    )
}

async fn probe_frame() -> Html<String> {
    page("Probe frame", "<p>frame</p>")
}

fn script(body: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript")], body)
}

/// The probe's routes, canned into every [`FixtureServer`](crate::FixtureServer).
pub(crate) fn routes() -> Router {
    Router::new()
        .route(PROBE_PATH, get(probe_page))
        .route("/probe/frame", get(probe_frame))
        .route("/probe/identity.js", get(|| async { script(IDENTITY_JS) }))
        .route("/probe/worker.js", get(|| async { script(WORKER_JS) }))
}

/// For each field, what each source claimed.
type Claims = BTreeMap<&'static str, Vec<(String, Value)>>;

fn claim(claims: &mut Claims, field: &'static str, source: impl Into<String>, value: Value) {
    claims
        .entry(field)
        .or_default()
        .push((source.into(), value));
}

/// The OS a user agent string names: `Windows`, `macOS`, `Android`,
/// `Chrome OS`, `Linux`, or what's left over.
fn os_of_user_agent(user_agent: &str) -> String {
    let os = if user_agent.contains("Windows") {
        "Windows"
    } else if user_agent.contains("Macintosh") {
        "macOS"
    } else if user_agent.contains("Android") {
        "Android"
    } else if user_agent.contains("CrOS") {
        "Chrome OS"
    } else if user_agent.contains("Linux") {
        "Linux"
    } else {
        return format!("unknown ({user_agent})");
    };
    os.to_string()
}

/// The OS a `navigator.platform` value implies, named as in
/// [`os_of_user_agent`].
fn os_of_platform(platform: &str) -> String {
    let os = if platform.starts_with("Win") {
        "Windows"
    } else if platform.starts_with("Mac") {
        "macOS"
    } else if platform.starts_with("Linux") {
        "Linux"
    } else {
        return format!("unknown ({platform})");
    };
    os.to_string()
}

/// The `os` claim of a string-valued `value`, through `of`.
fn os_claim(value: &Value, of: fn(&str) -> String) -> Value {
    value.as_str().map_or(Value::Null, |s| Value::String(of(s)))
}

/// What `realm`'s reading (one entry of [`PROBE_JS`]'s result) claims.
fn js_claims(claims: &mut Claims, realm: &str, id: &Value) {
    for field in [
        "userAgent",
        "platform",
        "hardwareConcurrency",
        "deviceMemory",
        "languages",
        "timeZone",
        "webgl",
    ] {
        claim(claims, field, realm, id[field].clone());
    }
    // A realm without `userAgentData` claims null for all of these.
    let data = &id["uaData"];
    claim(claims, "brands", realm, data["brands"].clone());
    claim(claims, "mobile", realm, data["mobile"].clone());
    claim(claims, "uaPlatform", realm, data["platform"].clone());
    let high = &id["uaDataHigh"];
    for field in [
        "architecture",
        "bitness",
        "fullVersionList",
        "model",
        "platformVersion",
        "wow64",
    ] {
        claim(claims, field, realm, high[field].clone());
    }

    let os = |what: &str| format!("{realm} {what}");
    claim(
        claims,
        "os",
        os("userAgent"),
        os_claim(&id["userAgent"], os_of_user_agent),
    );
    claim(
        claims,
        "os",
        os("platform"),
        os_claim(&id["platform"], os_of_platform),
    );
    claim(claims, "os", os("userAgentData"), data["platform"].clone());
}

/// The brands of a `Sec-CH-UA` or `Sec-CH-UA-Full-Version-List` header, as
/// `brand/version` strings like the JS side's. `None` if it doesn't parse.
fn parse_brand_list(header: &str) -> Option<Value> {
    let mut brands = Vec::new();
    let mut rest = header.trim();
    while !rest.is_empty() {
        let after_quote = rest.strip_prefix('"')?;
        let (brand, after_brand) = after_quote.split_once('"')?;
        let after_v = after_brand.strip_prefix(";v=\"")?;
        let (version, after_version) = after_v.split_once('"')?;
        brands.push(Value::String(format!("{brand}/{version}")));
        rest = after_version.trim_start_matches([',', ' ']);
    }
    Some(Value::Array(brands))
}

/// A structured-header string (`"x86"`) without its quotes.
fn unquote(header: &str) -> Value {
    Value::String(header.trim_matches('"').to_string())
}

/// A structured-header boolean (`?0` / `?1`).
fn sf_boolean(header: &str) -> Value {
    match header {
        "?0" => Value::Bool(false),
        "?1" => Value::Bool(true),
        other => Value::String(format!("not a boolean: {other}")),
    }
}

/// The language tags of an `Accept-Language` header, in order, without
/// their weights.
fn accept_language_tags(header: &str) -> Value {
    header
        .split(',')
        .map(|item| item.split(';').next().unwrap_or("").trim())
        .map(|tag| Value::String(tag.to_string()))
        .collect()
}

/// What `request`'s headers claim, labelled as `source`.
fn http_claims(claims: &mut Claims, source: &str, request: &RecordedRequest) {
    if let Some(ua) = request.header("user-agent") {
        claim(claims, "userAgent", source, json!(ua));
        claim(
            claims,
            "os",
            format!("{source} user-agent"),
            json!(os_of_user_agent(ua)),
        );
    }
    if let Some(brands) = request.header("sec-ch-ua") {
        let parsed = parse_brand_list(brands).unwrap_or_else(|| json!(brands));
        claim(claims, "brands", source, parsed);
    }
    if let Some(list) = request.header("sec-ch-ua-full-version-list") {
        let parsed = parse_brand_list(list).unwrap_or_else(|| json!(list));
        claim(claims, "fullVersionList", source, parsed);
    }
    if let Some(platform) = request.header("sec-ch-ua-platform") {
        claim(claims, "uaPlatform", source, unquote(platform));
        claim(
            claims,
            "os",
            format!("{source} sec-ch-ua-platform"),
            unquote(platform),
        );
    }
    for (header, field) in [
        ("sec-ch-ua-arch", "architecture"),
        ("sec-ch-ua-bitness", "bitness"),
        ("sec-ch-ua-model", "model"),
        ("sec-ch-ua-platform-version", "platformVersion"),
    ] {
        if let Some(value) = request.header(header) {
            claim(claims, field, source, unquote(value));
        }
    }
    for (header, field) in [("sec-ch-ua-mobile", "mobile"), ("sec-ch-ua-wow64", "wow64")] {
        if let Some(value) = request.header(header) {
            claim(claims, field, source, sf_boolean(value));
        }
    }
    if let Some(languages) = request.header("accept-language") {
        claim(claims, "languages", source, accept_language_tags(languages));
    }
}

/// Headers that at least one probe request must carry, or the HTTP side of
/// the comparison is missing. The high-entropy ones arrive only after the
/// page's `Accept-CH`.
const REQUIRED_HEADERS: &[&str] = &[
    "user-agent",
    "accept-language",
    "sec-ch-ua",
    "sec-ch-ua-platform",
    "sec-ch-ua-platform-version",
    "sec-ch-ua-full-version-list",
];

/// Every field on which the probe's sources disagree, each with what every
/// source claimed. Empty when they all agree.
///
/// - `probe`: what [`PROBE_JS`] returned.
/// - `requests`: the server's request log since the probe page was loaded,
///   e.g. [`FixtureServer::requests`](crate::FixtureServer::requests);
///   requests outside [`PROBE_PATH`] are ignored.
///
/// The fields: `userAgent`, `os` (from the UA string, `navigator.platform`,
/// `userAgentData.platform` and `Sec-CH-UA-Platform`), `platform`,
/// `hardwareConcurrency`, `deviceMemory`, `languages` (with
/// `Accept-Language`), `timeZone`, `webgl` (unmasked vendor and renderer),
/// and `userAgentData`'s `brands`, `mobile`, `uaPlatform`, `architecture`,
/// `bitness`, `fullVersionList`, `model`, `platformVersion` and `wow64`
/// (each with its `Sec-CH-UA-*` header).
///
/// Panics if a realm's reading is missing, or if no probe request carried
/// one of the headers the comparison needs: either means the probe didn't
/// run as intended, so an empty result would mean nothing.
pub fn disagreements(probe: &Value, requests: &[RecordedRequest]) -> Claims {
    let mut claims = Claims::new();
    for realm in ["main", "iframe", "worker"] {
        let id = &probe[realm];
        assert!(id.is_object(), "the probe has no {realm} reading: {probe}");
        js_claims(&mut claims, realm, id);
    }

    let probe_requests: Vec<(usize, &RecordedRequest)> = requests
        .iter()
        .enumerate()
        .filter(|(_, r)| r.uri.path().starts_with(PROBE_PATH))
        .collect();
    for header in REQUIRED_HEADERS {
        assert!(
            probe_requests
                .iter()
                .any(|(_, r)| r.header(header).is_some()),
            "no probe request carried {header}, so the comparison would skip it; is the probe \
             served from a secure origin such as 127.0.0.1?"
        );
    }
    for (index, request) in probe_requests {
        let source = format!("HTTP #{index} {}", request.uri.path());
        http_claims(&mut claims, &source, request);
    }

    claims.retain(|_, values| values.iter().any(|(_, v)| *v != values[0].1));
    claims
}

/// `disagreements` as readable text, one field per paragraph.
pub fn describe_disagreements(disagreements: &Claims) -> String {
    let mut text = String::new();
    for (field, values) in disagreements {
        text.push_str(field);
        text.push_str(":\n");
        for (source, value) in values {
            text.push_str(&format!("    {source}: {value}\n"));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_lists_parse_like_the_js_side() {
        assert_eq!(
            parse_brand_list(r#""Not)A;Brand";v="8", "Chromium";v="156", "Google Chrome";v="156""#),
            Some(json!([
                "Not)A;Brand/8",
                "Chromium/156",
                "Google Chrome/156"
            ])),
        );
        assert_eq!(parse_brand_list(""), Some(json!([])));
        assert_eq!(parse_brand_list(r#""Chromium";v=156"#), None);
    }

    #[test]
    fn accept_language_drops_weights() {
        assert_eq!(
            accept_language_tags("en-US,en;q=0.9,de;q=0.8"),
            json!(["en-US", "en", "de"])
        );
    }

    #[test]
    fn os_names_agree_across_sources() {
        let windows = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36";
        let linux = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36";
        let mac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36";
        assert_eq!(os_of_user_agent(windows), os_of_platform("Win32"));
        assert_eq!(os_of_user_agent(linux), os_of_platform("Linux x86_64"));
        assert_eq!(os_of_user_agent(mac), os_of_platform("MacIntel"));
        assert_eq!(os_of_user_agent(mac), "macOS");
    }
}
