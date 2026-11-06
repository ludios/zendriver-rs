//! Interception rules: which paused requests a rule fits ([`Match`]), and
//! what it does with them ([`Action`]).

use std::fmt;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::error::InterceptionError;
use crate::host_matcher::{HostMatcher, host_of};
use crate::types::{
    AbortReason, PauseKind, RequestInfo, RequestOverrides, ResourceType, ResponseInfo,
    ResponseOverrides,
};
use crate::url_pattern::UrlPattern;

/// Which URLs a [`Match`] fits.
#[derive(Debug, Clone)]
pub enum UrlMatch {
    /// The URLs a CDP wildcard pattern matches (see [`UrlPattern`]).
    Glob(UrlPattern),
    /// The URLs whose host is in the set, or is a subdomain of one in it.
    Hosts(Arc<HostMatcher>),
}

/// The paused requests an entry decides: one kind of pause, the URLs, and
/// optionally one resource type.
#[derive(Debug, Clone)]
pub struct Match {
    pause: PauseKind,
    url: UrlMatch,
    resource_type: Option<ResourceType>,
}

impl Match {
    /// Requests whose URL `glob` matches (CDP wildcard syntax; see
    /// [`UrlPattern`]), at the [`Request`](PauseKind::Request) pause, of any
    /// resource type.
    ///
    /// # Errors
    ///
    /// [`InterceptionError::InvalidPattern`] if `glob` doesn't compile.
    pub fn url(glob: impl Into<String>) -> Result<Self, InterceptionError> {
        Ok(Self {
            pause: PauseKind::Request,
            url: UrlMatch::Glob(UrlPattern::new(glob)?),
            resource_type: None,
        })
    }

    /// Requests whose host is in `hosts` or is a subdomain of one in it, at
    /// the [`Request`](PauseKind::Request) pause, of any resource type.
    #[must_use]
    pub fn hosts(hosts: Arc<HostMatcher>) -> Self {
        Self {
            pause: PauseKind::Request,
            url: UrlMatch::Hosts(hosts),
            resource_type: None,
        }
    }

    /// The same requests, at the `pause` kind instead.
    #[must_use]
    pub fn at(mut self, pause: PauseKind) -> Self {
        self.pause = pause;
        self
    }

    /// The same requests, of `resource_type` only. `XHR`, `Fetch` and
    /// `EventSource` are one type to Chrome, so each of them fits all three.
    /// Chrome refuses a pattern for `TextTrack`, `WebSocket`, `Manifest`,
    /// `SignedExchange` or `Preflight`, so inserting an entry of one of
    /// those fails with [`InterceptionError::Call`].
    #[must_use]
    pub fn resource(mut self, resource_type: ResourceType) -> Self {
        self.resource_type = Some(resource_type);
        self
    }

    /// The kind of pause this fits.
    pub fn pause(&self) -> PauseKind {
        self.pause
    }

    /// The URLs this fits.
    pub fn url_match(&self) -> &UrlMatch {
        &self.url
    }

    /// The resource type this fits, or `None` for any.
    pub fn resource_type(&self) -> Option<ResourceType> {
        self.resource_type
    }

    /// Whether a request for `url`, of `resource_type`, paused at `pause`,
    /// fits.
    pub(crate) fn fits(&self, url: &str, resource_type: ResourceType, pause: PauseKind) -> bool {
        let url_fits = match &self.url {
            UrlMatch::Glob(glob) => glob.matches(url),
            UrlMatch::Hosts(hosts) => host_of(url).is_some_and(|h| hosts.is_blocked(h)),
        };
        self.fits_any_url(resource_type, pause) && url_fits
    }

    /// Whether a request of `resource_type`, paused at `pause`, fits for
    /// some URL.
    pub(crate) fn fits_any_url(&self, resource_type: ResourceType, pause: PauseKind) -> bool {
        self.pause == pause && self.resource_type.is_none_or(|t| covers(t, resource_type))
    }

    /// The `Fetch.RequestPattern` that has Chrome pause the requests this
    /// fits (for a host set, every URL: CDP patterns can't name hosts).
    pub(crate) fn cdp_pattern(&self) -> Value {
        let mut pattern = Map::new();
        let url = match &self.url {
            UrlMatch::Glob(glob) => glob.pattern_str(),
            UrlMatch::Hosts(_) => "*",
        };
        pattern.insert("urlPattern".into(), Value::String(url.into()));
        if let Some(t) = self.resource_type {
            pattern.insert("resourceType".into(), Value::String(t.as_cdp_str().into()));
        }
        let stage = self.pause.as_cdp_stage();
        pattern.insert("requestStage".into(), Value::String(stage.into()));
        Value::Object(pattern)
    }
}

/// Whether a pattern for `pattern`-type requests covers one of `actual`
/// type. Chrome pauses XHR, Fetch and EventSource requests alike for a
/// pattern naming any of the three (they are one type to its loader; see
/// `AddInterceptedResourceType` in Chromium's `fetch_handler.cc`).
fn covers(pattern: ResourceType, actual: ResourceType) -> bool {
    let xhr_like = |t| {
        matches!(
            t,
            ResourceType::XHR | ResourceType::Fetch | ResourceType::EventSource
        )
    };
    pattern == actual || (xhr_like(pattern) && xhr_like(actual))
}

/// What an entry does with a paused request it decides.
pub enum Action {
    /// Let it go on unchanged: an explicit exception ahead of later entries.
    Continue,
    /// Fail it with `reason` (`Fetch.failRequest`).
    Block(AbortReason),
    /// Answer it with this response instead of the server's
    /// (`Fetch.fulfillRequest`).
    Respond {
        /// The HTTP status code.
        status: u16,
        /// The response headers, as `(name, value)` pairs.
        headers: Vec<(String, String)>,
        /// The raw body.
        body: Vec<u8>,
    },
    /// Send it to this absolute URL instead, unseen by the page
    /// (`Fetch.continueRequest`). [`Request`](PauseKind::Request) pauses
    /// only.
    RewriteUrl(String),
    /// Send it on rewritten by the closure's overrides
    /// (`Fetch.continueRequest`). [`Request`](PauseKind::Request) pauses
    /// only. The closure runs on the interception task: it must not block.
    ModifyRequest(Arc<dyn Fn(&RequestInfo) -> RequestOverrides + Send + Sync>),
    /// Let the response go on, keeping its body, rewritten by the closure's
    /// overrides (`Fetch.continueResponse`).
    /// [`ResponseHeaders`](PauseKind::ResponseHeaders) pauses only.
    ModifyResponse(Arc<dyn Fn(&ResponseInfo) -> ResponseOverrides + Send + Sync>),
}

impl Action {
    /// The action's name, as in its variant.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Continue => "Continue",
            Self::Block(_) => "Block",
            Self::Respond { .. } => "Respond",
            Self::RewriteUrl(_) => "RewriteUrl",
            Self::ModifyRequest(_) => "ModifyRequest",
            Self::ModifyResponse(_) => "ModifyResponse",
        }
    }

    /// Whether this can act on a request paused at `pause`.
    fn acts_on(&self, pause: PauseKind) -> bool {
        match self {
            Self::RewriteUrl(_) | Self::ModifyRequest(_) => pause == PauseKind::Request,
            Self::ModifyResponse(_) => pause == PauseKind::ResponseHeaders,
            Self::Continue | Self::Block(_) | Self::Respond { .. } => true,
        }
    }
}

// Hand-written because the closures aren't `Debug`.
impl fmt::Debug for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Block(reason) => f.debug_tuple("Block").field(reason).finish(),
            Self::Respond {
                status,
                headers,
                body,
            } => f
                .debug_struct("Respond")
                .field("status", status)
                .field("headers", headers)
                .field("body_len", &body.len())
                .finish(),
            Self::RewriteUrl(url) => f.debug_tuple("RewriteUrl").field(url).finish(),
            other => f.write_str(other.name()),
        }
    }
}

/// One interception rule: an [`Action`] for the paused requests that fit
/// a [`Match`].
#[derive(Debug)]
pub struct Rule {
    pub(crate) label: Option<String>,
    pub(crate) when: Match,
    pub(crate) action: Action,
}

impl Rule {
    /// A rule that does `action` with the requests `when` fits.
    ///
    /// # Errors
    ///
    /// [`InterceptionError::MismatchedPause`] if `action` can't act on the
    /// kind of pause `when` names: `RewriteUrl` and `ModifyRequest` need
    /// [`Request`](PauseKind::Request), `ModifyResponse` needs
    /// [`ResponseHeaders`](PauseKind::ResponseHeaders).
    pub fn new(when: Match, action: Action) -> Result<Self, InterceptionError> {
        if !action.acts_on(when.pause) {
            return Err(InterceptionError::MismatchedPause {
                action: action.name(),
                pause: when.pause,
            });
        }
        Ok(Self {
            label: None,
            when,
            action,
        })
    }

    /// The same rule, named `label` in snapshots.
    #[must_use]
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_match_fits_its_pause_kind_url_and_resource_type() {
        let api = Match::url("*/api/*").unwrap().resource(ResourceType::XHR);
        let request = PauseKind::Request;
        assert!(api.fits("https://a.test/api/x", ResourceType::Fetch, request));
        assert!(!api.fits("https://a.test/api/x", ResourceType::Image, request));
        assert!(!api.fits("https://a.test/x", ResourceType::XHR, request));
        let headers = PauseKind::ResponseHeaders;
        assert!(!api.fits("https://a.test/api/x", ResourceType::XHR, headers));

        let hosts = Match::hosts(Arc::new(HostMatcher::new(["evil.test".to_string()])));
        assert!(hosts.fits("https://cdn.evil.test/x.js", ResourceType::Script, request));
        assert!(!hosts.fits("https://good.test/x.js", ResourceType::Script, request));
    }

    #[test]
    fn a_match_asks_chrome_for_its_stage_and_type() {
        let at_response = Match::url("*/api/*")
            .unwrap()
            .at(PauseKind::ResponseError)
            .resource(ResourceType::Document);
        assert_eq!(
            at_response.cdp_pattern(),
            json!({ "urlPattern": "*/api/*", "resourceType": "Document", "requestStage": "Response" })
        );
        let hosts = Match::hosts(Arc::new(HostMatcher::new(Vec::new())));
        assert_eq!(
            hosts.cdp_pattern(),
            json!({ "urlPattern": "*", "requestStage": "Request" })
        );
    }

    #[test]
    fn a_rule_refuses_an_action_its_pause_kind_can_not_take() {
        let response = Match::url("*").unwrap().at(PauseKind::ResponseHeaders);
        let err = Rule::new(response, Action::RewriteUrl("https://b.test/".into())).unwrap_err();
        assert!(matches!(
            err,
            InterceptionError::MismatchedPause {
                action: "RewriteUrl",
                pause: PauseKind::ResponseHeaders
            }
        ));
        let modify = Action::ModifyResponse(Arc::new(|_| ResponseOverrides::default()));
        assert!(Rule::new(Match::url("*").unwrap(), modify).is_err());
        let error = Match::url("*").unwrap().at(PauseKind::ResponseError);
        assert!(Rule::new(error, Action::Block(AbortReason::Failed)).is_ok());
    }
}
