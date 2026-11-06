//! [`PausedRequest`] — the per-event handle handed to stream consumers.
//!
//! Each `Fetch.requestPaused` event surfaces a `PausedRequest`. Code must
//! dispatch exactly one of [`continue_`], [`abort`], [`respond`],
//! [`modify_and_continue`], or [`continue_response`] to release the paused
//! request — Chrome holds the request open until one of these arrives.
//! [`body`] is a read-only side-call usable at the `Response` stage to inspect
//! the upstream body before deciding what to do; it does not release the
//! pause.
//!
//! [`continue_`]: PausedRequest::continue_
//! [`abort`]: PausedRequest::abort
//! [`respond`]: PausedRequest::respond
//! [`modify_and_continue`]: PausedRequest::modify_and_continue
//! [`continue_response`]: PausedRequest::continue_response
//! [`body`]: PausedRequest::body
//!
//! A `PausedRequest` hands its answer to its session's interception task
//! (the multiplexer), which sends it to Chrome and sees it through: a caller
//! that stops waiting can't make the request be answered twice, and a
//! request continued to a new URL pauses at the Response stage exactly when
//! an entry fits the new URL. It also carries a [`SessionHandle`] (not a
//! full `Tab`), for [`body`], so the type lives in this crate without a
//! reverse dependency on `zendriver`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use zendriver_transport::SessionHandle;

use crate::error::InterceptionError;
use crate::fetch;
use crate::interceptor::{Answer, Command};
use crate::types::{
    AbortReason, PauseKind, RequestInfo, RequestOverrides, ResponseInfo, ResponseOverrides,
};

/// A request that Chrome paused and a stream entry decides.
///
/// `PausedRequest` is consumed by exactly one of the action methods
/// ([`continue_`](Self::continue_), [`abort`](Self::abort),
/// [`respond`](Self::respond),
/// [`modify_and_continue`](Self::modify_and_continue),
/// [`continue_response`](Self::continue_response)) to release the pause.
/// [`body`](Self::body) is a read-only side-channel (`&self`) usable at the
/// `Response` stage to inspect the upstream body before deciding which
/// terminal action to take.
///
/// If a `PausedRequest` is dropped without a terminal action (e.g. the
/// consuming task panicked, or a `select!` arm was cancelled before the
/// action was called), [`Drop`] has the multiplexer fail the request
/// (`Fetch.failRequest`, reason `Failed`), so Chrome doesn't hold it open
/// indefinitely. A terminal action that Chrome refuses is followed by the
/// same `Fetch.failRequest`. See cdpdriver/zendriver#126 for the freeze
/// pattern this prevents.

#[derive(Debug)]
pub struct PausedRequest {
    /// Opaque CDP request id (`requestId` on `Fetch.requestPaused`). Must be
    /// echoed back on whichever terminal action releases the pause.
    pub request_id: String,
    /// What the request is paused waiting on.
    pub pause: PauseKind,
    /// Decoded request payload as Chrome surfaced it.
    pub request: RequestInfo,
    /// Decoded response payload — populated only when Chrome paused at the
    /// `Response` stage. `None` at the `Request` stage.
    pub response: Option<ResponseInfo>,
    session: SessionHandle,
    /// The multiplexer's queue, which every answer goes through.
    answers: mpsc::UnboundedSender<Command>,
    /// Flipped to `true` by every terminal action (`continue_`, `abort`,
    /// `respond`, `modify_and_continue`, `continue_response`) as it hands
    /// its answer to the multiplexer, which sees it through from then on.
    /// [`Drop`] inspects this to decide whether to answer for the request
    /// itself — set means "already answered, don't double-fire"; clear
    /// means "not answered, release Chrome".
    released: bool,
}

impl PausedRequest {
    /// Construct a `PausedRequest` from the multiplexer. `pub(crate)` so
    /// the public API stays "consume one of the action methods" — callers
    /// never assemble a `PausedRequest` by hand.
    pub(crate) fn new(
        request_id: impl Into<String>,
        pause: PauseKind,
        request: RequestInfo,
        response: Option<ResponseInfo>,
        session: SessionHandle,
        answers: mpsc::UnboundedSender<Command>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            pause,
            request,
            response,
            session,
            answers,
            released: false,
        }
    }

    /// Release the pause and let Chrome send the request as-is.
    ///
    /// Dispatches `Fetch.continueRequest { requestId }` with no overrides.
    /// Use [`modify_and_continue`](Self::modify_and_continue) instead if any
    /// field needs to be rewritten.
    ///
    /// ```no_run
    /// # use futures::StreamExt;
    /// # async fn ex(tab: &zendriver_interception::Interceptor)
    /// #   -> Result<(), zendriver_interception::InterceptionError> {
    /// use zendriver_interception::{Match, Position};
    ///
    /// let mut stream = tab.subscribe(Match::url("*")?, Position::Last).await?;
    /// while let Some(req) = stream.next().await {
    ///     req.continue_().await?;
    /// }
    /// # Ok(()) }
    /// ```
    pub async fn continue_(self) -> Result<(), InterceptionError> {
        let params = fetch::continue_params(&self.request_id, RequestOverrides::default());
        self.release("Fetch.continueRequest", params).await
    }

    /// Abort the request with the given Chrome [`Network.ErrorReason`].
    ///
    /// Dispatches `Fetch.failRequest { requestId, errorReason }`. The exact
    /// reason surfaces to JS as the rejected `fetch()` / `XHR` error.
    ///
    /// ```no_run
    /// # use futures::StreamExt;
    /// # async fn ex(tab: &zendriver_interception::Interceptor)
    /// #   -> Result<(), zendriver_interception::InterceptionError> {
    /// use zendriver_interception::{AbortReason, Match, Position};
    ///
    /// let mut stream = tab.subscribe(Match::url("*")?, Position::Last).await?;
    /// while let Some(req) = stream.next().await {
    ///     req.abort(AbortReason::BlockedByClient).await?;
    /// }
    /// # Ok(()) }
    /// ```
    ///
    /// [`Network.ErrorReason`]: https://chromedevtools.github.io/devtools-protocol/tot/Network/#type-ErrorReason
    pub async fn abort(self, reason: AbortReason) -> Result<(), InterceptionError> {
        let params = fetch::fail_params(&self.request_id, reason);
        self.release("Fetch.failRequest", params).await
    }

    /// Synthesize a response and serve it in place of the upstream one.
    ///
    /// Dispatches `Fetch.fulfillRequest { requestId, responseCode,
    /// responseHeaders: [{name, value}, ...], body: base64(body) }`. Headers
    /// are CDP's name/value array form; `body` is base64-encoded on the wire
    /// per CDP spec (callers pass raw bytes).
    pub async fn respond(
        self,
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<(), InterceptionError> {
        let params = fetch::fulfill_params(&self.request_id, status, &headers, &body);
        self.release("Fetch.fulfillRequest", params).await
    }

    /// Release the pause with per-field overrides applied.
    ///
    /// Dispatches `Fetch.continueRequest { requestId, url?, method?, headers?,
    /// postData? }`. Only fields set on [`RequestOverrides`] are forwarded —
    /// `None` leaves Chrome's original value untouched. Per CDP, `postData`
    /// crosses the wire base64-encoded and `headers` is the CDP name/value
    /// array form (replacement, not merge).
    pub async fn modify_and_continue(
        self,
        overrides: RequestOverrides,
    ) -> Result<(), InterceptionError> {
        let params = fetch::continue_params(&self.request_id, overrides);
        self.release("Fetch.continueRequest", params).await
    }

    /// Forward the upstream response, optionally rewriting its status and/or
    /// headers, while keeping Chrome's original body.
    ///
    /// Only valid at the `Response` stage — Chrome only accepts
    /// `Fetch.continueResponse` once the upstream response headers have
    /// arrived. Called on a `Request`-stage `PausedRequest` (where
    /// [`response`](Self::response) is `None`), it returns
    /// [`InterceptionError::WrongStage`] without dispatching
    /// `continueResponse` (the request, dropped unanswered, is then
    /// failed); use
    /// [`respond`](Self::respond) to serve a synthetic response at the
    /// `Request` stage instead.
    ///
    /// Dispatches `Fetch.continueResponse`. An argument left `None` keeps the
    /// value in [`response`](Self::response): Chrome takes a status and
    /// headers together or neither, so once anything is overridden, those
    /// are sent for the ones left `None`. A new `status` without a `phrase`
    /// gets Chrome's standard phrase for it, and Chrome refuses a status
    /// code it has no phrase for (e.g. 299): give such a code a phrase
    /// (`""` for none). `headers` replaces the whole header block, it isn't
    /// merged in: pass every header you want forwarded.
    ///
    /// ```no_run
    /// # use futures::StreamExt;
    /// # async fn ex(tab: &zendriver_interception::Interceptor)
    /// #   -> Result<(), zendriver_interception::InterceptionError> {
    /// use zendriver_interception::{Match, PauseKind, Position};
    ///
    /// let responses = Match::url("*")?.at(PauseKind::ResponseHeaders);
    /// let mut stream = tab.subscribe(responses, Position::Last).await?;
    /// while let Some(req) = stream.next().await {
    ///     // Rewrite to 204 No Content, keep the upstream body.
    ///     req.continue_response(Some(204), None, None).await?;
    /// }
    /// # Ok(()) }
    /// ```
    pub async fn continue_response(
        self,
        status: Option<u16>,
        phrase: Option<String>,
        headers: Option<Vec<(String, String)>>,
    ) -> Result<(), InterceptionError> {
        // At the Request stage, Chrome refuses continueResponse; the
        // request is dropped unanswered, so Drop fails it.
        let Some(response) = self.response.as_ref() else {
            return Err(InterceptionError::WrongStage);
        };
        let overrides = ResponseOverrides {
            status,
            phrase,
            headers,
        };
        let params = fetch::continue_response_params(&self.request_id, response, overrides);
        self.release("Fetch.continueResponse", params).await
    }

    /// Hand the multiplexer `method` with `params`, which release this
    /// request, and wait for Chrome's reply.
    ///
    /// From then on the multiplexer sees the answer through, so it counts
    /// as released (and [`Drop`] sends nothing): a caller that stops
    /// waiting doesn't leave it half done, nor answered twice.
    ///
    /// # Errors
    ///
    /// [`InterceptionError::Call`] if Chrome refused the answer (the
    /// multiplexer then deals with the request as it does with a rule's
    /// refused answer), and [`InterceptionError::SubscriptionClosed`] if
    /// the multiplexer is gone.
    async fn release(
        mut self,
        method: &'static str,
        params: Value,
    ) -> Result<(), InterceptionError> {
        self.released = true;
        let (done, reply) = oneshot::channel();
        let answer = Command::Answer(Answer {
            request_id: self.request_id.clone(),
            resource_type: self.request.resource_type,
            method,
            params,
            done: Some(done),
        });
        if self.answers.send(answer).is_err() {
            return Err(InterceptionError::SubscriptionClosed);
        }
        reply
            .await
            .map_err(|_| InterceptionError::SubscriptionClosed)?
    }

    /// Fetch the upstream response body. Only useful at the `Response`
    /// stage — at the `Request` stage Chrome has no body to return.
    ///
    /// Dispatches `Fetch.getResponseBody { requestId }`. Per CDP the result
    /// carries a `body` string plus a `base64Encoded: bool` flag: when true,
    /// we base64-decode; when false, we return the UTF-8 bytes verbatim. Keeps
    /// `&self` (not `self`) so callers can inspect-then-decide.
    pub async fn body(&self) -> Result<Vec<u8>, InterceptionError> {
        let res = self
            .session
            .call(
                "Fetch.getResponseBody",
                json!({ "requestId": self.request_id }),
            )
            .await?;
        let body = res.get("body").and_then(Value::as_str).ok_or_else(|| {
            InterceptionError::InvalidResponse(
                "Fetch.getResponseBody returned no body field".into(),
            )
        })?;
        let base64_encoded = res
            .get("base64Encoded")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if base64_encoded {
            BASE64
                .decode(body)
                .map_err(|e| InterceptionError::InvalidResponse(format!("invalid base64: {e}")))
        } else {
            Ok(body.as_bytes().to_vec())
        }
    }
}

/// Safety net for cdpdriver/zendriver#126: a paused request that gets dropped
/// without a terminal action (panicked handler, cancelled `select!` arm,
/// stream consumer exited early) would otherwise leave Chrome waiting on
/// `requestId` forever and freeze the whole client. The drop has the
/// multiplexer fail it instead.
impl Drop for PausedRequest {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let request_id = std::mem::take(&mut self.request_id);
        // A multiplexer that is gone took the session's pauses with it.
        let _ = self.answers.send(Command::Dropped(request_id));
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt;
    use serde_json::json;
    use zendriver_transport::Connection;
    use zendriver_transport::testing::MockConnection;

    use super::*;
    use crate::interceptor::testing::{answered, applied, mock_interceptor, pause};
    use crate::interceptor::{InterceptStream, Position};
    use crate::rule::Match;

    /// A mock Chrome and its connection; a stream entry for every request at
    /// `pause_kind`; and the request `request_id` the stream got: an XHR,
    /// with (at a Response pause) a `200 OK` response without headers. The
    /// tests keep the stream: dropping it would change `Fetch`'s
    /// configuration.
    async fn paused(
        request_id: &str,
        pause_kind: PauseKind,
    ) -> (MockConnection, Connection, InterceptStream, PausedRequest) {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let when = Match::url("*").unwrap().at(pause_kind);
        let (mut stream, _) = applied(&mut mock, interceptor.subscribe(when, Position::Last)).await;
        let mut fields = json!({ "resourceType": "XHR" });
        if pause_kind == PauseKind::ResponseHeaders {
            fields["responseStatusCode"] = json!(200);
            fields["responseStatusText"] = json!("OK");
            fields["responseHeaders"] = json!([]);
        }
        pause(&mock, request_id, "https://example.test/widget", fields).await;
        let req = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        (mock, conn, stream, req)
    }

    /// The next command sent within 2 s, as its method and parameters,
    /// answered with `{}`.
    async fn next_sent(mock: &mut MockConnection) -> (String, Value) {
        let (method, id) = mock.recv_cmd_timeout(Duration::from_secs(2)).await.unwrap();
        let params = mock.last_sent()["params"].clone();
        mock.reply(id, json!({})).await;
        (method, params)
    }

    /// Whether no command is sent within 200 ms.
    async fn nothing_sent(mock: &mut MockConnection) -> bool {
        mock.recv_cmd_timeout(Duration::from_millis(200))
            .await
            .is_none()
    }

    /// `continue_` sends one `continueRequest`, with no overrides, and its
    /// drop (it consumes the request) sends nothing more.
    #[tokio::test]
    async fn continue_sends_one_plain_continue_request() {
        let (mut mock, conn, _stream, req) = paused("REQ-1", PauseKind::Request).await;
        let (sent, continued) = futures::join!(req.continue_(), next_sent(&mut mock));
        sent.unwrap();
        let plain = json!({ "requestId": "REQ-1" });
        assert_eq!(continued, ("Fetch.continueRequest".to_string(), plain));
        assert!(
            nothing_sent(&mut mock).await,
            "a command followed continue_"
        );
        conn.shutdown();
    }

    /// cdpdriver/zendriver#126: a `PausedRequest` dropped without a terminal
    /// action must still release Chrome, or the request stays paused for
    /// good and stalls its page. Its drop fails it.
    #[tokio::test]
    async fn a_request_dropped_unanswered_is_failed() {
        let (mut mock, conn, _stream, req) = paused("REQ-DROP", PauseKind::Request).await;
        drop(req);
        let failed = json!({ "requestId": "REQ-DROP", "errorReason": "Failed" });
        assert_eq!(answered(&mut mock, "Fetch.failRequest").await, failed);
        conn.shutdown();
    }

    /// A terminal action that Chrome refuses returns Chrome's error, and the
    /// request is failed.
    #[tokio::test]
    async fn a_refused_answer_is_followed_by_a_fail() {
        let (mut mock, conn, _stream, req) = paused("REQ-REFUSED", PauseKind::Request).await;
        let sent = tokio::spawn(req.continue_());
        let id = mock.expect_cmd("Fetch.continueRequest").await;
        mock.reply_err(id, -32602, "Invalid InterceptionId.").await;
        let failed = json!({ "requestId": "REQ-REFUSED", "errorReason": "Failed" });
        let next = ("Fetch.failRequest".to_string(), failed);
        assert_eq!(next_sent(&mut mock).await, next);
        assert!(sent.await.unwrap().is_err());
        conn.shutdown();
    }

    /// An action whose caller stops waiting while it is in flight leaves the
    /// answer to the multiplexer: the drop sends no answer of its own.
    #[tokio::test]
    async fn an_action_cancelled_in_flight_is_not_followed_by_another_answer() {
        let (mut mock, conn, _stream, req) = paused("REQ-CANCELLED", PauseKind::Request).await;
        let abort = tokio::spawn(req.abort(AbortReason::Failed));
        let id = mock.expect_cmd("Fetch.failRequest").await;
        abort.abort();
        assert!(abort.await.unwrap_err().is_cancelled());
        mock.reply(id, json!({})).await;
        assert!(
            nothing_sent(&mut mock).await,
            "a command followed the action"
        );
        conn.shutdown();
    }

    #[tokio::test]
    async fn respond_dispatches_fetch_fulfill_with_base64_body() {
        let (mut mock, conn, _stream, req) = paused("REQ-2", PauseKind::Request).await;
        let headers = vec![("content-type".into(), "text/plain".into())];
        let body = b"hello world".to_vec();
        let expected_b64 = BASE64.encode(&body);
        let (sent, (method, params)) =
            futures::join!(req.respond(200, headers, body), next_sent(&mut mock));
        sent.unwrap();
        assert_eq!(method, "Fetch.fulfillRequest");
        assert_eq!(params["requestId"], "REQ-2");
        assert_eq!(params["responseCode"], 200);
        assert_eq!(params["body"], expected_b64);
        assert_eq!(
            params["responseHeaders"],
            json!([{ "name": "content-type", "value": "text/plain" }])
        );
        conn.shutdown();
    }

    /// `continue_response` sends one `continueResponse`, leaving out the
    /// phrase it wasn't given (rather than sending `null`).
    #[tokio::test]
    async fn continue_response_sends_one_continue_response() {
        let (mut mock, conn, _stream, req) = paused("REQ-CR", PauseKind::ResponseHeaders).await;
        let headers = Some(vec![("x".into(), "y".into())]);
        let (sent, continued) = futures::join!(
            req.continue_response(Some(204), None, headers),
            next_sent(&mut mock)
        );
        sent.unwrap();
        let params = json!({
            "requestId": "REQ-CR",
            "responseCode": 204,
            "responseHeaders": [{ "name": "x", "value": "y" }],
        });
        assert_eq!(continued, ("Fetch.continueResponse".to_string(), params));
        assert!(nothing_sent(&mut mock).await, "a command followed it");
        conn.shutdown();
    }

    /// At a Request pause, `continue_response` sends nothing (Chrome would
    /// refuse it) and returns `WrongStage`; the request, dropped unanswered,
    /// is failed.
    #[tokio::test]
    async fn continue_response_at_a_request_pause_is_an_error() {
        let (mut mock, conn, _stream, req) = paused("REQ-WS", PauseKind::Request).await;
        let err = req
            .continue_response(Some(200), None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, InterceptionError::WrongStage));
        let failed = json!({ "requestId": "REQ-WS", "errorReason": "Failed" });
        let next = ("Fetch.failRequest".to_string(), failed);
        assert_eq!(next_sent(&mut mock).await, next);
        conn.shutdown();
    }
}
