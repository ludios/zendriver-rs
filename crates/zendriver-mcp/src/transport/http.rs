//! Streamable HTTP transport — per-session [`ZendriverServer`] factory
//! mounted on axum.
//!
//! Each incoming MCP session triggers the factory, which mints a fresh
//! [`SessionState`] (and therefore a fresh `Browser` slot). That keeps
//! HTTP clients fully isolated — one client opening a browser does not
//! affect another's tab state.

use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::Duration;

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio::sync::Mutex;

use crate::server::{ZendriverServer, close_owned_browser, shutdown_signal};
use crate::state::{SessionState, StealthProfileChoice};

/// How long the browsers of all sessions get to close after a shutdown
/// signal. A session's browser can only be taken while no tool call holds
/// the session, and `Browser::close` itself allows 15 s.
const CLEANUP_WITHIN: Duration = Duration::from_secs(20);

/// Bind a streamable HTTP MCP server on `addr` and serve until the process
/// receives a shutdown signal, then close every session's browser.
///
/// Each new MCP session is created with its own [`SessionState`] seeded
/// with `default_profile` — clients never share browser state.
pub async fn serve(
    addr: SocketAddr,
    default_profile: StealthProfileChoice,
) -> Result<(), Box<dyn std::error::Error>> {
    // Every session's state, to close its browser on a shutdown signal. Weak,
    // so a session that ends drops its state (and browser) as before.
    let sessions: Arc<std::sync::Mutex<Vec<Weak<Mutex<SessionState>>>>> = Arc::default();
    let registry = sessions.clone();
    let service = StreamableHttpService::new(
        move || {
            // Fresh SessionState per session keeps each client's Browser
            // / tabs isolated. The factory must be infallible — any
            // failure here would surface as an `io::Error` to the client
            // mid-handshake.
            let state = Arc::new(Mutex::new(SessionState {
                stealth_profile_choice: default_profile,
                ..SessionState::new()
            }));
            let mut registry = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.retain(|session| session.strong_count() > 0);
            registry.push(Arc::downgrade(&state));
            Ok(ZendriverServer { state })
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    // The bound address, not `addr`: with port 0 only this says which port.
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "zendriver-mcp HTTP listening on /mcp");
    tokio::select! {
        res = axum::serve(listener, router) => return Ok(res?),
        () = shutdown_signal() => {}
    }
    tracing::info!("shutdown signal received; closing every session's browser before exit");
    // A second signal means: stop waiting for the cleanup.
    tokio::spawn(async {
        shutdown_signal().await;
        tracing::warn!("second shutdown signal; exiting without finishing the cleanup");
        std::process::exit(1);
    });
    // The sessions run in tasks of their own, which outlive the server
    // future, so their states are still there to clean up.
    let states: Vec<_> = sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    let mut cleanup = tokio::task::JoinSet::new();
    for state in states {
        cleanup.spawn(async move { close_owned_browser(&state).await });
    }
    if tokio::time::timeout(CLEANUP_WITHIN, cleanup.join_all())
        .await
        .is_err()
    {
        tracing::warn!(
            "closing the sessions' browsers took over {CLEANUP_WITHIN:?}; exiting anyway"
        );
    }
    Ok(())
}
