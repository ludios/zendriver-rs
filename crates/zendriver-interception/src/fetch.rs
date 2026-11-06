//! The `Fetch` domain as this crate speaks it: the `Fetch.requestPaused`
//! event decoded, and the parameters of the commands that release a paused
//! request.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::{Map, Value, json};

use crate::types::{
    AbortReason, PauseKind, RequestInfo, RequestOverrides, ResourceType, ResponseInfo,
    ResponseOverrides,
};

/// Decoded `Fetch.requestPaused` event payload.
///
/// Projects only the fields this crate reads. Extra fields Chrome sends
/// (e.g. `frameId`, `networkId`) are deliberately ignored — the rule API
/// surfaces what callers asked for via [`RequestInfo`] / [`ResponseInfo`].
#[derive(Debug, Deserialize)]
pub(crate) struct RequestPausedEvent {
    #[serde(rename = "requestId")]
    pub(crate) request_id: String,
    pub(crate) request: RequestPayload,
    #[serde(rename = "resourceType", default)]
    pub(crate) resource_type: Option<String>,
    // Only populated at the `Response` stage.
    #[serde(rename = "responseStatusCode", default)]
    pub(crate) response_status_code: Option<u16>,
    #[serde(rename = "responseStatusText", default)]
    pub(crate) response_status_text: Option<String>,
    #[serde(rename = "responseHeaders", default)]
    pub(crate) response_headers: Option<Vec<HeaderPair>>,
    /// Set instead of the status when the response failed (Response stage).
    #[serde(rename = "responseErrorReason", default)]
    pub(crate) response_error_reason: Option<String>,
}

impl RequestPausedEvent {
    /// What the request is paused waiting on: its failure, or its
    /// response's status, or (with neither) being sent.
    pub(crate) fn pause_kind(&self) -> PauseKind {
        if self.response_error_reason.is_some() {
            PauseKind::ResponseError
        } else if self.response_status_code.is_some() {
            PauseKind::ResponseHeaders
        } else {
            PauseKind::Request
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RequestPayload {
    pub(crate) url: String,
    pub(crate) method: String,
    /// Request headers in the order the decoder yields them, kept as pairs
    /// rather than a map so nothing is reordered or merged on our side of the
    /// boundary. See [`deserialize_headers`] for what CDP can and cannot tell
    /// us about the original wire order.
    #[serde(default, deserialize_with = "deserialize_headers")]
    pub(crate) headers: Vec<(String, String)>,
    /// Chrome's text representation of the request body. For multipart /
    /// binary uploads this can be lossy — Chrome rebuilds via UTF-8 best
    /// effort. Prefer [`Self::post_data_entries`] when present.
    #[serde(rename = "postData", default)]
    pub(crate) post_data: Option<String>,
    #[serde(rename = "hasPostData", default)]
    _has_post_data: Option<bool>,
    /// Per-chunk base64-encoded bytes. Chrome emits this for binary /
    /// multipart bodies where the text representation would be lossy.
    /// When present, it is the canonical source of truth for the body.
    #[serde(rename = "postDataEntries", default)]
    pub(crate) post_data_entries: Option<Vec<PostDataEntry>>,
}

/// Decode CDP's `Network.Headers` (a JSON *object*) into ordered pairs,
/// preserving whatever order the deserializer hands us.
///
/// Why this is not the browser's true header order: CDP models request
/// headers as an object keyed by name, so duplicate-named headers are already
/// merged by Chrome before the event is serialized, and JSON object order is
/// only as faithful as the parser that read it. Our transport hands events to
/// this crate as an already-parsed [`serde_json::Value`], whose object map is
/// a `BTreeMap` unless `serde_json/preserve_order` is enabled workspace-wide —
/// so the names arrive sorted, and the wire order is gone before we see it.
///
/// Collecting into a `Vec` here is still worth doing: it is deterministic
/// (a `HashMap` would re-randomize the order on every request, which is a
/// worse fingerprint for a caller who round-trips `RequestInfo::headers` back
/// through `RequestOverrides`), and it is the piece that would automatically
/// become order-faithful if the parse ever starts preserving order. Restoring
/// the real order needs a raw-bytes event path in `zendriver-transport`.
fn deserialize_headers<'de, D>(deserializer: D) -> Result<Vec<(String, String)>, D::Error>
where
    D: Deserializer<'de>,
{
    struct HeadersVisitor;

    impl<'de> Visitor<'de> for HeadersVisitor {
        type Value = Vec<(String, String)>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a map of header name to header value")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut headers = Vec::with_capacity(map.size_hint().unwrap_or(0));
            while let Some(pair) = map.next_entry::<String, String>()? {
                headers.push(pair);
            }
            Ok(headers)
        }
    }

    deserializer.deserialize_map(HeadersVisitor)
}

#[derive(Debug, Deserialize)]
pub(crate) struct PostDataEntry {
    /// Base64-encoded body bytes. Per CDP `Network.PostDataEntry`.
    #[serde(default)]
    pub(crate) bytes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct HeaderPair {
    pub(crate) name: String,
    pub(crate) value: String,
}

/// Build a [`RequestInfo`] from the decoded event for `Modify` closures.
///
/// Body precedence: `postDataEntries` (canonical, base64-decoded +
/// concatenated) when present, else `postData` interpreted as UTF-8 bytes. The
/// string fallback is necessarily lossy for binary bodies — Chrome only emits
/// `postDataEntries` when it knows the text form would mangle the bytes. The
/// entries path is all-or-nothing: see [`decode_post_data`].
///
/// Headers come from `Network.Request.headers`, which CDP models as an object
/// keyed by header name — so duplicates are pre-merged by Chrome and the
/// browser's on-the-wire header *order* does not survive to this point. See
/// [`deserialize_headers`] for the full explanation; the pairs are passed
/// through unchanged here.
pub(crate) fn build_request_info(ev: &RequestPausedEvent) -> RequestInfo {
    RequestInfo {
        url: ev.request.url.clone(),
        method: ev.request.method.clone(),
        headers: ev.request.headers.clone(),
        post_data: decode_post_data(&ev.request),
        resource_type: parse_resource_type(ev.resource_type.as_deref()),
    }
}

/// Decode the request body, all-or-nothing.
///
/// `postDataEntries` is a chunked body: the caller only has the real payload
/// if *every* chunk decodes. If any entry is unusable (absent `bytes`, or
/// invalid base64) we return `None` rather than the successfully-decoded
/// prefix — a partial body is indistinguishable from a complete one, so a
/// `Modify` closure that signs or hashes it would produce a valid signature
/// over the wrong bytes. We also don't fall back to `postData` in that case:
/// its lossy UTF-8 rebuild would be a different set of plausible-but-wrong
/// bytes. `None` is the only honest answer the [`RequestInfo::post_data`]
/// shape can give.
///
/// [`RequestInfo::post_data`]: crate::types::RequestInfo::post_data
fn decode_post_data(req: &RequestPayload) -> Option<Vec<u8>> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;

    if let Some(entries) = req.post_data_entries.as_ref() {
        let mut buf = Vec::new();
        for entry in entries {
            let Some(b64) = entry.bytes.as_deref() else {
                tracing::warn!(
                    "interception: postDataEntries entry without bytes; reporting no body rather than a truncated one"
                );
                return None;
            };
            match BASE64.decode(b64) {
                Ok(bytes) => buf.extend_from_slice(&bytes),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "interception: bad base64 in postDataEntries; reporting no body rather than a truncated one"
                    );
                    return None;
                }
            }
        }
        return Some(buf);
    }
    req.post_data.as_deref().map(|s| s.as_bytes().to_vec())
}

/// Build a [`ResponseInfo`] from the decoded event when Chrome paused at the
/// `Response` stage. Returns `None` at the `Request` stage (the event
/// payload's `responseStatusCode` is absent).
///
pub(crate) fn build_response_info(ev: &RequestPausedEvent) -> Option<ResponseInfo> {
    let status = ev.response_status_code?;
    let status_text = ev.response_status_text.clone().unwrap_or_default();
    let headers: Vec<(String, String)> = ev
        .response_headers
        .as_ref()
        .map(|hs| {
            hs.iter()
                .map(|h| (h.name.clone(), h.value.clone()))
                .collect()
        })
        .unwrap_or_default();
    Some(ResponseInfo {
        status,
        status_text,
        headers,
    })
}

/// Serialize a `[(name, value)]` slice into CDP's `[{name, value}]` JSON
/// array shape used by `Fetch.continueRequest.headers` and
/// `Fetch.fulfillRequest.responseHeaders`.
pub(crate) fn headers_to_cdp(headers: &[(String, String)]) -> Vec<Value> {
    headers
        .iter()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect()
}

/// Best-effort parse of a CDP `Network.ResourceType` string into our enum.
/// Defaults to [`ResourceType::Other`] for unknown strings rather than
/// failing the whole event — Chrome occasionally adds new types we don't
/// know about yet, and dropping a real intercepted request for that would
/// be a worse failure mode than reporting `Other`.
pub(crate) fn parse_resource_type(s: Option<&str>) -> ResourceType {
    match s.unwrap_or("Other") {
        "Document" => ResourceType::Document,
        "Stylesheet" => ResourceType::Stylesheet,
        "Image" => ResourceType::Image,
        "Media" => ResourceType::Media,
        "Font" => ResourceType::Font,
        "Script" => ResourceType::Script,
        "TextTrack" => ResourceType::TextTrack,
        "XHR" => ResourceType::XHR,
        "Fetch" => ResourceType::Fetch,
        "EventSource" => ResourceType::EventSource,
        "WebSocket" => ResourceType::WebSocket,
        "Manifest" => ResourceType::Manifest,
        "SignedExchange" => ResourceType::SignedExchange,
        "Ping" => ResourceType::Ping,
        "CSPViolationReport" => ResourceType::CSPViolationReport,
        "Preflight" => ResourceType::Preflight,
        _ => ResourceType::Other,
    }
}

/// The `Fetch.failRequest` parameters that fail `request_id` with `reason`.
pub(crate) fn fail_params(request_id: &str, reason: AbortReason) -> Value {
    json!({ "requestId": request_id, "errorReason": reason.as_cdp_str() })
}

/// The `Fetch.fulfillRequest` parameters that answer `request_id` with a
/// response of our own: `status`, `headers` as `(name, value)` pairs, and
/// the raw `body`.
pub(crate) fn fulfill_params(
    request_id: &str,
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
) -> Value {
    json!({
        "requestId": request_id,
        "responseCode": status,
        "responseHeaders": headers_to_cdp(headers),
        "body": BASE64.encode(body),
    })
}

/// The `Fetch.continueRequest` parameters that send `request_id` on,
/// rewritten by `overrides` (a `None` field keeps Chrome's value).
pub(crate) fn continue_params(request_id: &str, overrides: RequestOverrides) -> Value {
    let mut params = Map::new();
    params.insert("requestId".into(), Value::String(request_id.into()));
    if let Some(url) = overrides.url {
        params.insert("url".into(), Value::String(url));
    }
    if let Some(method) = overrides.method {
        params.insert("method".into(), Value::String(method));
    }
    if let Some(headers) = overrides.headers {
        params.insert("headers".into(), Value::Array(headers_to_cdp(&headers)));
    }
    if let Some(post_data) = overrides.post_data {
        params.insert("postData".into(), Value::String(BASE64.encode(&post_data)));
    }
    Value::Object(params)
}

/// The `Fetch.continueResponse` parameters that release `request_id`, paused
/// with the upstream `response`, rewritten by `overrides`.
///
/// Chrome takes a status and headers together or neither, so once anything
/// is overridden, the status and headers that `overrides` leaves `None` are
/// copied from `response`. With the upstream status, a phrase left `None` is
/// the upstream phrase; with a new status, it is left to Chrome, which sends
/// its standard phrase for that code. An empty phrase (every HTTP/2
/// response's) is sent as `" "`: Chrome refuses `""`, and trims the spaces
/// off the status line, so the page sees an empty phrase.
pub(crate) fn continue_response_params(
    request_id: &str,
    response: &ResponseInfo,
    overrides: ResponseOverrides,
) -> Value {
    let ResponseOverrides {
        status,
        mut phrase,
        headers,
    } = overrides;
    let mut params = Map::new();
    params.insert("requestId".into(), Value::String(request_id.into()));
    if status.is_none() && phrase.is_none() && headers.is_none() {
        return Value::Object(params);
    }
    let status = status.unwrap_or(response.status);
    if phrase.is_none() && status == response.status {
        phrase = Some(response.status_text.clone());
    }
    let headers = headers.as_deref().unwrap_or(&response.headers);
    params.insert("responseCode".into(), Value::from(status));
    params.insert(
        "responseHeaders".into(),
        Value::Array(headers_to_cdp(headers)),
    );
    if let Some(phrase) = phrase {
        let phrase = if phrase.is_empty() {
            " ".into()
        } else {
            phrase
        };
        params.insert("responsePhrase".into(), Value::String(phrase));
    }
    Value::Object(params)
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// `continueResponse` gets a status and headers together or neither: an
    /// override of one is completed from the upstream response, whose
    /// phrase goes only with its own status.
    #[test]
    fn continue_response_params_send_status_and_headers_together() {
        let response = ResponseInfo {
            status: 200,
            status_text: "Fine".into(),
            headers: vec![("content-type".into(), "text/html".into())],
        };
        let params = |overrides| continue_response_params("R1", &response, overrides);
        let upstream_headers = json!([{ "name": "content-type", "value": "text/html" }]);

        assert_eq!(
            params(ResponseOverrides::default()),
            json!({ "requestId": "R1" })
        );
        assert_eq!(
            params(ResponseOverrides {
                status: Some(203),
                ..ResponseOverrides::default()
            }),
            json!({ "requestId": "R1", "responseCode": 203, "responseHeaders": upstream_headers })
        );
        assert_eq!(
            params(ResponseOverrides {
                headers: Some(vec![("x".into(), "1".into())]),
                ..ResponseOverrides::default()
            }),
            json!({
                "requestId": "R1",
                "responseCode": 200,
                "responsePhrase": "Fine",
                "responseHeaders": [{ "name": "x", "value": "1" }],
            })
        );
        assert_eq!(
            params(ResponseOverrides {
                phrase: Some("Swell".into()),
                ..ResponseOverrides::default()
            }),
            json!({
                "requestId": "R1",
                "responseCode": 200,
                "responsePhrase": "Swell",
                "responseHeaders": upstream_headers,
            })
        );
        // The upstream status, named again, keeps the upstream phrase.
        assert_eq!(
            params(ResponseOverrides {
                status: Some(200),
                ..ResponseOverrides::default()
            })["responsePhrase"],
            "Fine"
        );

        // An empty phrase, upstream (as on HTTP/2) or asked for, goes as " ".
        let h2 = ResponseInfo {
            status: 520,
            status_text: String::new(),
            headers: Vec::new(),
        };
        let headers_only = ResponseOverrides {
            headers: Some(Vec::new()),
            ..ResponseOverrides::default()
        };
        let sent = continue_response_params("R1", &h2, headers_only);
        assert_eq!(sent["responsePhrase"], " ");
        let empty = params(ResponseOverrides {
            phrase: Some(String::new()),
            ..ResponseOverrides::default()
        });
        assert_eq!(empty["responsePhrase"], " ");
    }

    /// Decode a `Fetch.requestPaused` payload the way the multiplexer does.
    fn request_info_from(request: Value) -> RequestInfo {
        let ev: RequestPausedEvent = serde_json::from_value(json!({
            "requestId": "REQ-1",
            "request": request,
            "resourceType": "XHR",
        }))
        .expect("event fixture must decode");
        build_request_info(&ev)
    }

    /// Happy path: every `postDataEntries` chunk decodes, so the body is the
    /// concatenation. Guards the all-or-nothing fix below from over-correcting.
    #[test]
    fn post_data_entries_concatenate_when_every_chunk_decodes() {
        let info = request_info_from(json!({
            "url": "https://example.test/upload",
            "method": "POST",
            "headers": {},
            "postDataEntries": [
                { "bytes": BASE64.encode("hello ") },
                { "bytes": BASE64.encode("world") },
            ],
        }));
        assert_eq!(info.post_data.as_deref(), Some(&b"hello world"[..]));
    }

    /// A corrupt chunk must yield `None`, never the decoded prefix: a partial
    /// body is indistinguishable from a complete one, so a `Modify` closure
    /// that signs it would emit a valid signature over the wrong bytes.
    #[test]
    fn post_data_entries_with_corrupt_chunk_yield_none_not_a_partial_body() {
        let info = request_info_from(json!({
            "url": "https://example.test/upload",
            "method": "POST",
            "headers": {},
            "postDataEntries": [
                { "bytes": BASE64.encode("hello ") },
                { "bytes": "!!!not base64!!!" },
                { "bytes": BASE64.encode("world") },
            ],
        }));
        assert_eq!(
            info.post_data, None,
            "a truncated body must not surface as Some(prefix)"
        );
    }

    /// Same contract for a chunk Chrome sent without `bytes` — the body is
    /// incomplete, so we report no body rather than the surrounding chunks.
    #[test]
    fn post_data_entry_without_bytes_yields_none() {
        let info = request_info_from(json!({
            "url": "https://example.test/upload",
            "method": "POST",
            "headers": {},
            "postDataEntries": [
                { "bytes": BASE64.encode("hello ") },
                {},
            ],
        }));
        assert_eq!(info.post_data, None);
    }

    /// The `postData` fallback is untouched by the all-or-nothing rule — it
    /// only applies to the chunked `postDataEntries` path.
    #[test]
    fn post_data_string_fallback_still_decodes() {
        let info = request_info_from(json!({
            "url": "https://example.test/form",
            "method": "POST",
            "headers": {},
            "postData": "a=1&b=2",
        }));
        assert_eq!(info.post_data.as_deref(), Some(&b"a=1&b=2"[..]));
    }

    /// Request headers decode into a deterministic pair list. CDP's name-keyed
    /// object means we can't recover Chrome's wire order (see
    /// `deserialize_headers`), but two decodes of the same event must agree —
    /// a `HashMap` re-randomized the order per request, which a caller
    /// round-tripping `RequestInfo::headers` back through `RequestOverrides`
    /// turns into a randomized header order on the wire.
    #[test]
    fn request_headers_decode_in_a_deterministic_order() {
        let headers = json!({
            "host": "example.test",
            "user-agent": "zd/1",
            "accept": "*/*",
            "accept-language": "en-US",
            "accept-encoding": "gzip",
            "cookie": "a=1",
            "referer": "https://example.test/",
            "sec-fetch-mode": "cors",
        });
        let request = json!({
            "url": "https://example.test/x",
            "method": "GET",
            "headers": headers,
        });

        let first = request_info_from(request.clone()).headers;
        let second = request_info_from(request).headers;
        assert_eq!(
            first, second,
            "header order must not change between decodes of the same event"
        );
        assert_eq!(first.len(), 8, "every header must survive the decode");
        assert!(first.contains(&("cookie".to_string(), "a=1".to_string())));
    }
}
