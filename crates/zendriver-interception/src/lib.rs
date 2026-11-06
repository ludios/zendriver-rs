//! Network interception via the `Fetch` CDP domain.
//!
//! See the [Interception chapter](https://turtiesocks.github.io/zendriver-rs/interception.html)
//! of the [zendriver-rs user guide](https://turtiesocks.github.io/zendriver-rs/)
//! for narrative examples, glob/regex pattern semantics, and streaming-mode
//! recipes.
//!
//! A session's [`Interceptor`] owns its `Fetch` domain. Each
//! [`InterceptBuilder`] made for it adds one layer of interception, in one
//! of two ways:
//!
//! - **Rule-based** — chain
//!   [`block`](builder::InterceptBuilder::block) /
//!   [`redirect`](builder::InterceptBuilder::redirect) /
//!   [`respond`](builder::InterceptBuilder::respond) /
//!   [`modify_request`](builder::InterceptBuilder::modify_request) and call
//!   [`start`](builder::InterceptBuilder::start). Dropping the returned
//!   [`InterceptHandle`] removes the layer.
//! - **Stream** — call
//!   [`subscribe`](builder::InterceptBuilder::subscribe) to get an
//!   [`InterceptStream`] of [`PausedRequest`]s; release each pause by
//!   calling one of [`continue_`](PausedRequest::continue_),
//!   [`abort`](PausedRequest::abort), [`respond`](PausedRequest::respond),
//!   or [`modify_and_continue`](PausedRequest::modify_and_continue).
//!
//! Layers of one session compose: see [`interceptor`] for how a paused
//! request is handed to them.
//!
//! ```no_run
//! # async fn ex(session: zendriver_transport::SessionHandle)
//! #   -> Result<(), zendriver_interception::InterceptionError> {
//! use zendriver_interception::{InterceptBuilder, Interceptor};
//!
//! let interceptor = Interceptor::new(session);
//! let _handle = InterceptBuilder::new(&interceptor)
//!     .block("*/ads/*")?
//!     .redirect("*/old/*", "https://example.com/new/")?
//!     .start()
//!     .await?;
//! // _handle stays in scope -> interception live.
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

pub use builder::{InterceptBuilder, RequestPattern};
pub use error::InterceptionError;
pub use host_matcher::{HostMatcher, host_of};
pub use interceptor::{InterceptHandle, InterceptStream, Interceptor};
pub use paused::PausedRequest;
pub use rule::Rule;
pub use types::{
    AbortReason, RequestInfo, RequestOverrides, RequestStage, ResourceType, ResponseInfo,
    ResponseOverrides,
};
