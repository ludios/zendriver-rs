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
/// action was called), [`Drop`] has the multiplexer send a
/// `Fetch.continueRequest`, so Chrome doesn't hold the request open
/// indefinitely. A terminal action that Chrome refuses is followed by the
/// same `Fetch.continueRequest`. See cdpdriver/zendriver#126 for the
/// freeze pattern this prevents.

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
    /// `continueResponse` (the request, dropped unreleased, is then
    /// continued unchanged); use
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
        // request is dropped unreleased, so Drop continues it.
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
/// `requestId` forever and freeze the whole client. The drop hands the
/// multiplexer a plain `Fetch.continueRequest` for it instead.
impl Drop for PausedRequest {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let request_id = std::mem::take(&mut self.request_id);
        let params = fetch::continue_params(&request_id, RequestOverrides::default());
        let answer = Command::Answer(Answer {
            request_id,
            resource_type: self.request.resource_type,
            method: "Fetch.continueRequest",
            params,
            done: None,
        });
        // A multiplexer that is gone took the session's pauses with it.
        let _ = self.answers.send(answer);
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::interceptor::Interceptor;
    use crate::types::ResourceType;
    use zendriver_transport::SessionHandle;
    use zendriver_transport::testing::MockConnection;

    fn make_request_info() -> RequestInfo {
        RequestInfo {
            url: "https://example.test/widget".into(),
            method: "GET".into(),
            headers: Vec::new(),
            post_data: None,
            resource_type: ResourceType::XHR,
        }
    }

    /// A paused request of `session`, answered through a multiplexer of its
    /// own.
    fn paused(
        request_id: &str,
        pause: PauseKind,
        request: RequestInfo,
        response: Option<ResponseInfo>,
        session: SessionHandle,
    ) -> PausedRequest {
        let answers = Interceptor::new(session.clone()).commands().clone();
        PausedRequest::new(request_id, pause, request, response, session, answers)
    }

    fn make_response_info() -> ResponseInfo {
        ResponseInfo {
            status: 200,
            status_text: "OK".into(),
            headers: Vec::new(),
        }
    }

    #[tokio::test]
    async fn continue_dispatches_fetch_continue_request() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let req = paused("REQ-1", PauseKind::Request, make_request_info(), None, sess);

        let fut = tokio::spawn(async move { req.continue_().await });

        let id = mock.expect_cmd("Fetch.continueRequest").await;
        let sent = mock.last_sent();
        assert_eq!(sent["params"]["requestId"], "REQ-1");
        // No overrides on plain continue_: requestId is the only param.
        let params_obj = sent["params"].as_object().unwrap();
        assert_eq!(params_obj.len(), 1);
        mock.reply(id, serde_json::json!({})).await;

        fut.await.unwrap().unwrap();
        conn.shutdown();
    }

    #[tokio::test]
    async fn drop_without_terminal_action_fires_fallback_continue() {
        // cdpdriver/zendriver#126: a PausedRequest that goes out of scope
        // without continue_/abort/respond/modify_and_continue must release
        // Chrome — otherwise the InterceptionId stays armed forever and
        // freezes the next render.
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        {
            let req = paused(
                "REQ-DROP",
                PauseKind::Request,
                make_request_info(),
                None,
                sess,
            );
            drop(req);
        }
        // The Drop spawns the call on the current runtime; expect_cmd waits
        // for it to land.
        let id = mock.expect_cmd("Fetch.continueRequest").await;
        let sent = mock.last_sent();
        assert_eq!(sent["params"]["requestId"], "REQ-DROP");
        mock.reply(id, serde_json::json!({})).await;
        conn.shutdown();
    }

    /// A terminal action that Chrome refuses leaves the request unreleased,
    /// so its drop continues it.
    #[tokio::test]
    async fn a_refused_release_is_continued_on_drop() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let req = paused(
            "REQ-REFUSED",
            PauseKind::Request,
            make_request_info(),
            None,
            sess,
        );
        let abort = tokio::spawn(req.abort(AbortReason::Failed));
        let id = mock.expect_cmd("Fetch.failRequest").await;
        mock.reply_err(id, -32602, "Invalid InterceptionId.").await;
        let id = mock.expect_cmd("Fetch.continueRequest").await;
        assert_eq!(mock.last_sent()["params"]["requestId"], "REQ-REFUSED");
        mock.reply(id, serde_json::json!({})).await;
        assert!(abort.await.unwrap().is_err());
        conn.shutdown();
    }

    /// An action whose caller stops waiting while it is in flight leaves
    /// the release to it: the drop sends no `continueRequest` of its own.
    #[tokio::test]
    async fn an_action_cancelled_in_flight_is_not_followed_by_a_continue() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let req = paused(
            "REQ-CANCELLED",
            PauseKind::Request,
            make_request_info(),
            None,
            sess,
        );
        let abort = tokio::spawn(req.abort(AbortReason::Failed));
        mock.expect_cmd("Fetch.failRequest").await;
        abort.abort();
        assert!(abort.await.unwrap_err().is_cancelled());
        let next = mock
            .recv_cmd_timeout(std::time::Duration::from_millis(200))
            .await;
        assert_eq!(next, None, "a command followed the cancelled action");
        conn.shutdown();
    }

    #[tokio::test]
    async fn continue_does_not_double_fire_on_drop() {
        // After continue_() consumes self and returns, Drop runs on the moved
        // value. `released = true` set by continue_() must suppress the Drop
        // fallback — otherwise every successful path would send two CDP
        // round-trips.
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let req = paused(
            "REQ-ONCE",
            PauseKind::Request,
            make_request_info(),
            None,
            sess,
        );

        let fut = tokio::spawn(async move { req.continue_().await });

        let id = mock.expect_cmd("Fetch.continueRequest").await;
        assert_eq!(mock.last_sent()["params"]["requestId"], "REQ-ONCE");
        mock.reply(id, serde_json::json!({})).await;
        fut.await.unwrap().unwrap();

        // Give the runtime a tick — if Drop had spawned a second call it
        // would be queued by now. `try_recv_cmd` returns None if the channel
        // is empty.
        tokio::task::yield_now().await;
        assert!(
            mock.try_recv_cmd().is_none(),
            "Drop fired a second Fetch.continueRequest after continue_ already released"
        );
        conn.shutdown();
    }

    #[tokio::test]
    async fn respond_dispatches_fetch_fulfill_with_base64_body() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let req = paused("REQ-2", PauseKind::Request, make_request_info(), None, sess);

        let body = b"hello world".to_vec();
        let expected_b64 = BASE64.encode(&body);
        let fut = tokio::spawn(async move {
            req.respond(
                200,
                vec![("content-type".into(), "text/plain".into())],
                body,
            )
            .await
        });

        let id = mock.expect_cmd("Fetch.fulfillRequest").await;
        let sent = mock.last_sent();
        assert_eq!(sent["params"]["requestId"], "REQ-2");
        assert_eq!(sent["params"]["responseCode"], 200);
        assert_eq!(sent["params"]["body"], expected_b64);
        let headers = sent["params"]["responseHeaders"].as_array().unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0]["name"], "content-type");
        assert_eq!(headers[0]["value"], "text/plain");
        mock.reply(id, serde_json::json!({})).await;

        fut.await.unwrap().unwrap();
        conn.shutdown();
    }

    #[tokio::test]
    async fn continue_response_dispatches_fetch_continue_response() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let req = paused(
            "REQ-CR",
            PauseKind::ResponseHeaders,
            make_request_info(),
            Some(make_response_info()),
            sess,
        );

        let fut = tokio::spawn(async move {
            req.continue_response(Some(204), None, Some(vec![("x".into(), "y".into())]))
                .await
        });

        let id = mock.expect_cmd("Fetch.continueResponse").await;
        let sent = mock.last_sent();
        assert_eq!(sent["params"]["requestId"], "REQ-CR");
        assert_eq!(sent["params"]["responseCode"], 204);
        // `phrase: None` must be omitted entirely, not sent as null.
        assert!(
            sent["params"]
                .as_object()
                .unwrap()
                .get("responsePhrase")
                .is_none()
        );
        let headers = sent["params"]["responseHeaders"].as_array().unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0]["name"], "x");
        assert_eq!(headers[0]["value"], "y");
        mock.reply(id, serde_json::json!({})).await;

        fut.await.unwrap().unwrap();
        conn.shutdown();
    }

    #[tokio::test]
    async fn continue_response_wrong_stage_errs() {
        // Called on a Request-stage PausedRequest (response: None), Chrome
        // would reject continueResponse — we short-circuit with WrongStage
        // and dispatch nothing.
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let req = paused(
            "REQ-WS",
            PauseKind::Request,
            make_request_info(),
            None,
            sess,
        );

        let err = req
            .continue_response(Some(200), None, None)
            .await
            .expect_err("continue_response at Request stage must error");
        assert!(matches!(err, InterceptionError::WrongStage));

        // No Fetch.continueResponse was sent. (A Drop fallback
        // continueRequest *may* land — that's the freeze-safety net, not a
        // continueResponse dispatch.) Give the runtime a tick for any spawned
        // Drop task to enqueue, then drain whatever queued.
        tokio::task::yield_now().await;
        while let Some((method, _id)) = mock.try_recv_cmd() {
            assert_ne!(
                method, "Fetch.continueResponse",
                "WrongStage path must not dispatch Fetch.continueResponse"
            );
        }
        conn.shutdown();
    }

    #[tokio::test]
    async fn continue_response_no_double_fire_on_drop() {
        // After continue_response() consumes self and returns, Drop runs on
        // the moved value. `released = true` set by continue_response() must
        // suppress the Drop fallback continueRequest — otherwise the happy
        // path would send two CDP round-trips.
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let req = paused(
            "REQ-CR-ONCE",
            PauseKind::ResponseHeaders,
            make_request_info(),
            Some(make_response_info()),
            sess,
        );

        let fut = tokio::spawn(async move { req.continue_response(Some(200), None, None).await });

        let id = mock.expect_cmd("Fetch.continueResponse").await;
        assert_eq!(mock.last_sent()["params"]["requestId"], "REQ-CR-ONCE");
        mock.reply(id, serde_json::json!({})).await;
        fut.await.unwrap().unwrap();

        tokio::task::yield_now().await;
        assert!(
            mock.try_recv_cmd().is_none(),
            "Drop fired a fallback continueRequest after continue_response already released"
        );
        conn.shutdown();
    }
}
