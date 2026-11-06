//! Network interception via the `Fetch` CDP domain.
//!
//! See the [Interception chapter](https://turtiesocks.github.io/zendriver-rs/interception.html)
//! of the [zendriver-rs user guide](https://turtiesocks.github.io/zendriver-rs/)
//! for narrative examples, glob/regex pattern semantics, and streaming-mode
//! recipes.
//!
//! A session's [`Interceptor`] owns its `Fetch` domain: its interception
//! is one ordered list of entries, each one [`Rule`] (a [`Match`] and an
//! [`Action`]) or one stream, and the first entry that fits a paused
//! request decides it. Every insertion says where in the list it goes
//! ([`Position`]); see [`interceptor`] for the whole procedure.
//!
//! - **Rules** — [`Interceptor::insert`] them, or chain
//!   [`InterceptBuilder`]'s shorthand ([`block`](InterceptBuilder::block),
//!   [`redirect`](InterceptBuilder::redirect),
//!   [`respond`](InterceptBuilder::respond),
//!   [`modify_request`](InterceptBuilder::modify_request),
//!   [`modify_response`](InterceptBuilder::modify_response), ...) and call
//!   [`start`](InterceptBuilder::start). Dropping the returned
//!   [`InterceptHandle`] removes them.
//! - **Stream** — [`Interceptor::subscribe`] returns an [`InterceptStream`]
//!   of [`PausedRequest`]s; release each by calling one of
//!   [`continue_`](PausedRequest::continue_),
//!   [`abort`](PausedRequest::abort), [`respond`](PausedRequest::respond),
//!   [`modify_and_continue`](PausedRequest::modify_and_continue), or (at a
//!   `ResponseHeaders` pause) [`continue_response`](PausedRequest::continue_response).
//!
//! ```no_run
//! # async fn ex(session: zendriver_transport::SessionHandle)
//! #   -> Result<(), zendriver_interception::InterceptionError> {
//! use zendriver_interception::{InterceptBuilder, Interceptor, Position};
//!
//! let interceptor = Interceptor::new(session);
//! let _handle = InterceptBuilder::new(&interceptor)
//!     .block("*/ads/*")?
//!     .redirect("*/old/*", "https://example.com/new/")?
//!     .start(Position::Last)
//!     .await?;
//! // _handle stays in scope -> the rules stay in.
//! # Ok(()) }
//! ```

pub mod builder;
pub mod error;
mod fetch;
pub mod host_matcher;
pub mod interceptor;
pub mod paused;
pub mod rule;
pub mod types;
pub mod url_pattern;

pub use builder::InterceptBuilder;
pub use error::InterceptionError;
pub use host_matcher::{HostMatcher, host_of};
pub use interceptor::{
    ActionKind, AuthPause, Entry, InterceptHandle, InterceptStream, Interceptor, Position,
    ProxyAuth, RuleId, Snapshot, UrlInfo,
};
pub use paused::PausedRequest;
pub use rule::{Action, Match, Rule, UrlMatch};
pub use types::{
    AbortReason, PauseKind, RequestInfo, RequestOverrides, ResourceType, ResponseInfo,
    ResponseOverrides,
};
pub use url_pattern::UrlPattern;
