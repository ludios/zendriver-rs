//! Network-event fast path for Imperva clearance detection.
//!
//! Opt-in via [`ImpervaBypass::with_interception`]. Watches
//! `Network.responseReceived` for a 2xx response from an Imperva sensor
//! (`Reese.js` or `_Incapsula_Resource` in the URL) and signals the waiter
//! through a oneshot. Polling continues in parallel; the first signal wins.
//! Nothing is paused, so no request can be left waiting.
//!
//! [`ImpervaBypass::with_interception`]: crate::bypass::ImpervaBypass::with_interception

use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::oneshot;
use zendriver_transport::SessionHandle;

/// What the URL of an Imperva sensor contains.
const SENSOR_URL_PARTS: [&str; 2] = ["Reese.js", "_Incapsula_Resource"];

/// The part of a `Network.responseReceived` event that is read here.
#[derive(Deserialize)]
struct ResponseReceived {
    response: Response,
}

#[derive(Deserialize)]
struct Response {
    url: String,
    status: u16,
}

/// Watch `session` for the first 2xx response from an Imperva sensor.
///
/// Returns a receiver that gets `()` when that response arrives (its sender
/// is dropped if the session's events end first), and a guard that stops
/// the watch when dropped. A failed `Network.enable` is only logged: the
/// session may have Network enabled already (a `Tab`'s always has).
pub(crate) fn spawn_signal(session: &SessionHandle) -> (oneshot::Receiver<()>, SignalGuard) {
    let (tx, rx) = oneshot::channel();
    // Subscribed before `Network.enable`, so no response between the two is
    // missed.
    let mut responses = session.subscribe::<ResponseReceived>("Network.responseReceived");
    let session = session.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = session.call("Network.enable", json!({})).await {
            tracing::warn!(error = %e, "imperva: Network.enable failed; watching for sensor responses anyway");
        }
        while let Some(ResponseReceived { response }) = responses.next().await {
            let is_sensor = SENSOR_URL_PARTS.iter().any(|p| response.url.contains(p));
            if is_sensor && (200..300).contains(&response.status) {
                let _ = tx.send(());
                return;
            }
        }
    });
    (rx, SignalGuard(task))
}

/// Stops the watch started by [`spawn_signal`] when dropped.
pub(crate) struct SignalGuard(tokio::task::JoinHandle<()>);

impl Drop for SignalGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use super::*;
    use zendriver_transport::testing::MockConnection;

    /// The signal comes from a sensor's 2xx response, not from another
    /// URL's 2xx or a sensor's 403, even when `Network.enable` failed, and
    /// nothing is paused: the only command sent is `Network.enable`.
    #[tokio::test]
    async fn a_sensor_2xx_response_signals_and_nothing_is_paused() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let (mut rx, _guard) = spawn_signal(&sess);

        let enable = mock.recv_cmd_timeout(Duration::from_secs(2)).await;
        let (method, id) = enable.expect("no command sent");
        assert_eq!(method, "Network.enable");
        mock.reply_err(id, -32000, "Network domain is busy").await;

        let respond = |url: &str, status: u16| json!({ "requestId": "R", "response": { "url": url, "status": status } });
        for (url, status) in [
            ("https://site.test/app.js", 200),
            ("https://site.test/_Incapsula_Resource?SWJIYLWA=1", 403),
        ] {
            mock.emit_event_for_session("Network.responseReceived", respond(url, status), "S1")
                .await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "signalled without a sensor 2xx");

        mock.emit_event_for_session(
            "Network.responseReceived",
            respond("https://site.test/x/Reese.js", 200),
            "S1",
        )
        .await;
        tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("no signal after a sensor 2xx")
            .unwrap();
        assert_eq!(
            mock.recv_cmd_timeout(Duration::from_millis(50)).await,
            None,
            "a command was sent after Network.enable"
        );
        conn.shutdown();
    }
}
