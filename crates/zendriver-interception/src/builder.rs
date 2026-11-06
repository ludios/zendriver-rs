//! [`InterceptBuilder`]: shorthand for inserting several [`Rule`]s at once.
//!
//! Each method adds one rule, and [`start`](InterceptBuilder::start) puts
//! them into the session's interception, consecutively and in call order, at
//! the [`Position`] it is given, under one [`InterceptHandle`]. A rule the
//! methods can't express goes in with [`rule`](InterceptBuilder::rule).
//!
//! `Tab::intercept()` in `zendriver` builds on the tab's [`Interceptor`].

use std::sync::Arc;

use crate::error::InterceptionError;
use crate::host_matcher::HostMatcher;
use crate::interceptor::{InterceptHandle, Interceptor, Position};
use crate::rule::{Action, Match, Rule};
use crate::types::{
    AbortReason, PauseKind, RequestInfo, RequestOverrides, ResponseInfo, ResponseOverrides,
};

/// Fluent shorthand for inserting several rules into a session's
/// interception at once.
///
/// Every method that takes a `glob` matches it against the request's URL
/// (CDP wildcard syntax; see [`crate::UrlPattern`]) at the
/// [`Request`](PauseKind::Request) pause, for any resource type, except
/// [`modify_response`](Self::modify_response), which matches at the
/// [`ResponseHeaders`](PauseKind::ResponseHeaders) pause. Those methods
/// fail with [`InterceptionError::InvalidPattern`] for a `glob` that
/// doesn't compile.
//
// `Debug` works because `Action` has a hand-written `Debug` impl, which
// writes a closure action as its variant's name.
#[derive(Debug)]
pub struct InterceptBuilder {
    interceptor: Interceptor,
    rules: Vec<Rule>,
}

impl InterceptBuilder {
    /// A builder for rules of `interceptor`'s session.
    ///
    /// End users go through `Tab::intercept()` rather than calling this
    /// directly.
    ///
    /// ```no_run
    /// # async fn ex(interceptor: &zendriver_interception::Interceptor)
    /// #   -> Result<(), zendriver_interception::InterceptionError> {
    /// use zendriver_interception::{InterceptBuilder, Position};
    ///
    /// let _handle = InterceptBuilder::new(interceptor)
    ///     .block("*/tracker.js")?
    ///     .start(Position::Last)
    ///     .await?;
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn new(interceptor: &Interceptor) -> Self {
        Self {
            interceptor: interceptor.clone(),
            rules: Vec::new(),
        }
    }

    /// Add `rule`, for what the other methods can't express (a host set at
    /// another pause, a resource type, a label, ...).
    #[must_use]
    pub fn rule(mut self, rule: Rule) -> Self {
        self.rules.push(rule);
        self
    }

    /// Add a rule that lets the requests `glob` matches go on unchanged
    /// ([`Action::Continue`]): an explicit exception ahead of later entries.
    pub fn allow(self, glob: impl Into<String>) -> Result<Self, InterceptionError> {
        self.request_rule(glob, Action::Continue)
    }

    /// Add a rule that fails the requests `glob` matches with
    /// `BlockedByClient`, the error an ad blocker raises.
    pub fn block(self, glob: impl Into<String>) -> Result<Self, InterceptionError> {
        self.request_rule(glob, Action::Block(AbortReason::BlockedByClient))
    }

    /// Add a rule that fails, with `BlockedByClient`, the requests whose host
    /// is in `hosts` or is a subdomain of one in it: a set lookup per label
    /// of the request's host, however many hosts. `BrowserBuilder::block_trackers` in the
    /// `zendriver` crate installs such a rule for you.
    #[must_use]
    pub fn block_hosts(self, hosts: Arc<HostMatcher>) -> Self {
        // `Block` acts on any pause, so this needs no `Rule::new` check.
        self.rule(Rule {
            label: None,
            when: Match::hosts(hosts),
            action: Action::Block(AbortReason::BlockedByClient),
        })
    }

    /// Add a rule that sends the requests `glob` matches to the absolute URL
    /// `to` instead, unseen by the page.
    pub fn redirect(
        self,
        glob: impl Into<String>,
        to: impl Into<String>,
    ) -> Result<Self, InterceptionError> {
        self.request_rule(glob, Action::RewriteUrl(to.into()))
    }

    /// Add a rule that answers the requests `glob` matches with a response
    /// of `status`, `headers` (as `(name, value)` pairs) and the raw `body`,
    /// instead of the server's.
    pub fn respond(
        self,
        glob: impl Into<String>,
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<Self, InterceptionError> {
        let respond = Action::Respond {
            status,
            headers,
            body,
        };
        self.request_rule(glob, respond)
    }

    /// Add a rule that sends the requests `glob` matches on, rewritten by
    /// the overrides `modify` returns for each.
    ///
    /// The closure runs on the session's interception task per request, so
    /// it must not block, and must be `Send + Sync + 'static` (wrap shared
    /// state in `Arc`). Header overrides replace the request's headers
    /// (CDP semantics): return every header you want sent.
    pub fn modify_request<F>(
        self,
        glob: impl Into<String>,
        modify: F,
    ) -> Result<Self, InterceptionError>
    where
        F: Fn(&RequestInfo) -> RequestOverrides + Send + Sync + 'static,
    {
        self.request_rule(glob, Action::ModifyRequest(Arc::new(modify)))
    }

    /// Add a rule that lets the responses to the requests `glob` matches go
    /// on, keeping their bodies, rewritten by the overrides `modify` returns
    /// for each; at the [`ResponseHeaders`](PauseKind::ResponseHeaders)
    /// pause.
    ///
    /// As for [`modify_request`](Self::modify_request), the closure must not
    /// block and must be `Send + Sync + 'static`. Header overrides replace
    /// the response's headers: return every header you want forwarded.
    pub fn modify_response<F>(
        self,
        glob: impl Into<String>,
        modify: F,
    ) -> Result<Self, InterceptionError>
    where
        F: Fn(&ResponseInfo) -> ResponseOverrides + Send + Sync + 'static,
    {
        let when = Match::url(glob)?.at(PauseKind::ResponseHeaders);
        Ok(self.rule(Rule::new(when, Action::ModifyResponse(Arc::new(modify)))?))
    }

    /// Insert the rules into the session's interception at `at`,
    /// consecutively and in the order they were added, and return once
    /// Chrome pauses the requests they fit: a request sent from then on is
    /// decided by them (or by an entry ahead of them).
    ///
    /// The rules stay in until the returned [`InterceptHandle`] is dropped
    /// or [stopped](InterceptHandle::stop).
    ///
    /// # Errors
    ///
    /// As for [`Interceptor::insert`]: [`InterceptionError::NoRules`] with no
    /// rules, [`InterceptionError::NoSuchRule`] for a `Before` / `After`
    /// naming no entry, and [`InterceptionError::Call`] if Chrome refused
    /// the patterns or the session is gone.
    pub async fn start(self, at: Position) -> Result<InterceptHandle, InterceptionError> {
        self.interceptor.insert(self.rules, at).await
    }

    /// Add a rule of `action` for the requests `glob` matches at the
    /// `Request` pause.
    fn request_rule(
        self,
        glob: impl Into<String>,
        action: Action,
    ) -> Result<Self, InterceptionError> {
        Ok(self.rule(Rule::new(Match::url(glob)?, action)?))
    }

    /// Test-only accessor: the number of rules added.
    #[cfg(test)]
    pub(crate) fn rules_count(&self) -> usize {
        self.rules.len()
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::interceptor::testing::{applied, mock_interceptor};

    /// Every method adds one rule, and `start` asks Chrome for each rule's
    /// pattern, at its pause, in call order.
    #[tokio::test]
    async fn each_method_adds_one_rule_with_its_own_pattern() {
        let (mut mock, conn, interceptor) = mock_interceptor();
        let hosts = Arc::new(HostMatcher::new(["evil.test".to_string()]));
        let at_error = Match::url("*/error/*")
            .unwrap()
            .at(PauseKind::ResponseError);
        let builder = InterceptBuilder::new(&interceptor)
            .allow("*/allow/*")
            .unwrap()
            .block("*/ads/*")
            .unwrap()
            .redirect("*/old/*", "https://b.test/new")
            .unwrap()
            .respond("*/mock/*", 204, Vec::new(), Vec::new())
            .unwrap()
            .modify_request("*/req/*", |_| RequestOverrides::default())
            .unwrap()
            .modify_response("*/api/*", |_| ResponseOverrides::default())
            .unwrap()
            .block_hosts(hosts)
            .rule(Rule::new(at_error, Action::Continue).unwrap());
        assert_eq!(builder.rules_count(), 8);
        let (_handle, enable) = applied(&mut mock, builder.start(Position::Last)).await;
        let request = |glob: &str| json!({ "urlPattern": glob, "requestStage": "Request" });
        let response = |glob: &str| json!({ "urlPattern": glob, "requestStage": "Response" });
        assert_eq!(
            enable["patterns"],
            json!([
                request("*/allow/*"),
                request("*/ads/*"),
                request("*/old/*"),
                request("*/mock/*"),
                request("*/req/*"),
                response("*/api/*"),
                request("*"),
                response("*/error/*"),
            ])
        );
        conn.shutdown();
    }

    /// A builder with no rules has nothing to insert.
    #[tokio::test]
    async fn starting_no_rules_is_an_error() {
        let (_mock, conn, interceptor) = mock_interceptor();
        let err = InterceptBuilder::new(&interceptor)
            .start(Position::Last)
            .await
            .unwrap_err();
        assert!(matches!(err, InterceptionError::NoRules));
        conn.shutdown();
    }
}
