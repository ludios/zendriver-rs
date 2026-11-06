//! Interception tools — `browser_intercept_add_rule / _move_rule /
//! _remove_rule / _list_rules / _clear_rules`. Gated behind the
//! `interception` feature.
//!
//! ## The current tab's list
//!
//! A tab's interception is one ordered list of entries, and the first entry
//! that fits a paused request decides it (see `zendriver::interception`).
//! `browser_intercept_add_rule` inserts one rule into the current tab's
//! list at the `position` it is given, and keeps the returned
//! [`InterceptHandle`] in [`SessionState::rules`], under the rule's id. The
//! list may hold entries the agent didn't add: the browser's tracker
//! blocking (`browser.trackers`). `browser_intercept_list_rules` shows the
//! whole list, in order, each entry with its origin; every tool that
//! changes the list returns it, read after the change.
//!
//! A `rule_id` is the core [`RuleId`]'s decimal string: ids are never
//! reused, in any tab.
//!
//! ## Action surface (`InterceptAction`)
//!
//! Six tagged variants, one per core [`Action`]: `continue`, `block`,
//! `redirect`, `respond`, `modify_request`, `modify_response`. `respond`
//! carries a UTF-8 `body` plus an optional `content_type` (collapsed into
//! the headers map when set); binary bodies are out of scope.
//!
//! `modify_request` accepts a headers map of replacements; CDP's
//! `continueRequest.headers` is a *full replacement*, so the closure
//! returns a [`RequestOverrides`] that preserves the original headers and
//! overlays the caller-supplied entries. It also accepts optional `url` /
//! `method` / `post_data` overrides, passed straight through to
//! [`RequestOverrides`].
//!
//! [`InterceptHandle`]: zendriver::InterceptHandle
//! [`RequestOverrides`]: zendriver::RequestOverrides
//! [`RuleId`]: zendriver::interception::RuleId
//! [`Action`]: zendriver::interception::Action

#![cfg(feature = "interception")]

use std::collections::BTreeMap;
use std::sync::Arc;

use rmcp::ErrorData;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use zendriver::interception::{
    Action, ActionKind, AuthPause, Entry, HostMatcher, Match, PauseKind, Position, Rule, RuleId,
    UrlInfo,
};
use zendriver::{
    AbortReason, InterceptionError, RequestInfo, RequestOverrides, ResourceType, ResponseInfo,
    ResponseOverrides, ZendriverError,
};

use crate::errors::{McpServerError, map_error};
use crate::state::{InterceptRuleHandle, SessionState};
use crate::tools::common::current_tab;

// ---------- shared types --------------------------------------------------

/// Wire-level interception action. One variant per supported rule kind.
///
/// Tagged with `kind` so a JSON payload looks like
/// `{ "kind": "block" }` or `{ "kind": "redirect", "to": "..." }`, which
/// is easier for an agent to construct than a flat field set with mutually
/// exclusive shapes.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum InterceptAction {
    /// Let every matching request go on unchanged: an exception that keeps
    /// the matching requests from the entries after this rule.
    Continue,
    /// Fail every matching request with `BlockedByClient`.
    Block,
    /// Send every matching request to `to` instead, unseen by the page.
    /// `request` pause only.
    Redirect {
        /// Target URL, sent as it is: nothing of the matched URL is
        /// substituted into it.
        to: String,
    },
    /// Synthesize a response with the given status / body / headers.
    Respond {
        /// HTTP status code (e.g. `200`, `404`).
        status: u16,
        /// Response body as UTF-8 text.
        body: String,
        /// Convenience for the most common header: when set, a
        /// `Content-Type: <content_type>` header is added (and overrides
        /// any same-named entry in `headers`).
        #[serde(default)]
        content_type: Option<String>,
        /// Additional response headers. Lexicographically sorted (`BTreeMap`)
        /// for stable wire output.
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    /// Continue the request with extra / replaced headers and/or a replaced
    /// URL / method / body. Any field left `None` keeps Chrome's original.
    /// `request` pause only.
    ModifyRequest {
        /// Headers to merge over the request's originals. CDP's
        /// `continueRequest.headers` is a full replacement, so the closure
        /// rebuilds the header list from the intercepted request and
        /// overlays these entries (case-insensitive on names).
        #[serde(default)]
        headers: BTreeMap<String, String>,
        /// Replace the request URL. Omit to keep the original.
        #[serde(default)]
        url: Option<String>,
        /// Replace the HTTP method (e.g. `"POST"`). Omit to keep the
        /// original.
        #[serde(default)]
        method: Option<String>,
        /// Replace the request body, as UTF-8 text. Omit to keep the
        /// original. The core [`RequestOverrides::post_data`] field is
        /// `Vec<u8>`; this is encoded via `String::into_bytes`, so it can't
        /// carry a non-UTF-8 body.
        #[serde(default)]
        post_data: Option<String>,
    },
    /// Continue the *response* with an overridden status and/or headers.
    /// Other fields are left at Chrome's originals. The body is not
    /// rewritten — use `respond` to synthesize a whole response instead.
    /// `response_headers` pause only.
    ModifyResponse {
        /// Override the HTTP status code. Omit to keep Chrome's original.
        #[serde(default)]
        status: Option<u16>,
        /// Headers to merge over the response's originals (full-replacement
        /// semantics, same as `modify_request`; case-insensitive names).
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

impl InterceptAction {
    /// The action's `kind` on the wire.
    fn kind(&self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Block => "block",
            Self::Redirect { .. } => "redirect",
            Self::Respond { .. } => "respond",
            Self::ModifyRequest { .. } => "modify_request",
            Self::ModifyResponse { .. } => "modify_response",
        }
    }

    /// The core action that does what this one describes.
    fn into_action(self) -> Action {
        match self {
            Self::Continue => Action::Continue,
            Self::Block => Action::Block(AbortReason::BlockedByClient),
            Self::Redirect { to } => Action::RewriteUrl(to),
            Self::Respond {
                status,
                body,
                content_type,
                mut headers,
            } => {
                if let Some(ct) = content_type {
                    headers.retain(|name, _| !name.eq_ignore_ascii_case("content-type"));
                    headers.insert("content-type".into(), ct);
                }
                Action::Respond {
                    status,
                    headers: headers.into_iter().collect(),
                    body: body.into_bytes(),
                }
            }
            Self::ModifyRequest {
                headers,
                url,
                method,
                post_data,
            } => {
                // Captured once into an `Arc`, shared by every request the
                // rule decides (the closure must be `Send + Sync + 'static`).
                let overlay = Arc::new(ModifyRequestOverlay {
                    headers,
                    url,
                    method,
                    post_data,
                });
                Action::ModifyRequest(Arc::new(move |req: &RequestInfo| {
                    overlay.build(&req.headers)
                }))
            }
            Self::ModifyResponse { status, headers } => {
                let overlay = Arc::new(headers);
                Action::ModifyResponse(Arc::new(move |resp: &ResponseInfo| {
                    let headers = if overlay.is_empty() {
                        None
                    } else {
                        Some(merge_header_list(&resp.headers, &overlay))
                    };
                    ResponseOverrides {
                        status,
                        headers,
                        ..ResponseOverrides::default()
                    }
                }))
            }
        }
    }
}

/// The kind of pause a rule decides.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PauseName {
    /// Before the request is sent.
    Request,
    /// Once the response's status and headers arrived (4xx and 5xx
    /// included).
    ResponseHeaders,
    /// When the request failed before any status (DNS, connection, ...).
    ResponseError,
}

impl PauseName {
    /// The name on the wire.
    fn name(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::ResponseHeaders => "response_headers",
            Self::ResponseError => "response_error",
        }
    }

    fn pause_kind(self) -> PauseKind {
        match self {
            Self::Request => PauseKind::Request,
            Self::ResponseHeaders => PauseKind::ResponseHeaders,
            Self::ResponseError => PauseKind::ResponseError,
        }
    }

    fn of(pause: PauseKind) -> Self {
        match pause {
            PauseKind::Request => Self::Request,
            PauseKind::ResponseHeaders => Self::ResponseHeaders,
            PauseKind::ResponseError => Self::ResponseError,
        }
    }
}

/// A Chrome resource type (CDP `Network.ResourceType`), of those Chrome
/// can pause by type. `XHR`, `Fetch` and `EventSource` are one type to
/// Chrome: each fits all three.
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub enum ResourceTypeName {
    Document,
    Stylesheet,
    Image,
    Media,
    Font,
    Script,
    XHR,
    Fetch,
    EventSource,
    Ping,
    CSPViolationReport,
    Other,
}

impl ResourceTypeName {
    fn resource_type(self) -> ResourceType {
        match self {
            Self::Document => ResourceType::Document,
            Self::Stylesheet => ResourceType::Stylesheet,
            Self::Image => ResourceType::Image,
            Self::Media => ResourceType::Media,
            Self::Font => ResourceType::Font,
            Self::Script => ResourceType::Script,
            Self::XHR => ResourceType::XHR,
            Self::Fetch => ResourceType::Fetch,
            Self::EventSource => ResourceType::EventSource,
            Self::Ping => ResourceType::Ping,
            Self::CSPViolationReport => ResourceType::CSPViolationReport,
            Self::Other => ResourceType::Other,
        }
    }
}

/// Where in the current tab's list a rule goes, in the list as it is at
/// that moment: `"first"`, `"last"`, `{ "before": "<rule_id>" }` or
/// `{ "after": "<rule_id>" }`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum PositionInput {
    /// Ahead of every entry, `browser.trackers` included.
    First,
    /// Behind every entry.
    Last,
    /// Just ahead of the entry with this `rule_id`.
    Before(String),
    /// Just behind the entry with this `rule_id`.
    After(String),
}

impl PositionInput {
    fn position(self) -> Result<Position, ErrorData> {
        Ok(match self {
            Self::First => Position::First,
            Self::Last => Position::Last,
            Self::Before(id) => Position::Before(rule_id(&id)?),
            Self::After(id) => Position::After(rule_id(&id)?),
        })
    }
}

/// Who put an entry in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleOrigin {
    /// The browser, for an option of `browser_open` (`browser.trackers`,
    /// for tracker blocking).
    Browser,
    /// `browser_intercept_add_rule`.
    Agent,
}

/// What an entry does with a request it decides.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionSummary {
    /// Lets it go on unchanged.
    Continue,
    /// Fails it.
    Block {
        /// The CDP `Network.ErrorReason` it fails with.
        reason: String,
    },
    /// Answers it with a response of `status`.
    Respond {
        /// The response's HTTP status code.
        status: u16,
    },
    /// Sends it to `to` instead.
    Redirect {
        /// The URL it is sent to.
        to: String,
    },
    /// Sends it on with overrides.
    ModifyRequest,
    /// Lets the response go on with overrides.
    ModifyResponse,
    /// Hands it to a stream (`Interceptor::subscribe`, from code embedding
    /// the server), whose reader answers it.
    Stream,
}

/// One entry of the current tab's list.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RuleSummary {
    /// Where the entry is in the list, from 0. A paused request walks the
    /// list from index 0, and the first entry that fits it decides it.
    pub index: usize,
    /// The entry's id, for `position`, `browser_intercept_move_rule` and
    /// `browser_intercept_remove_rule`.
    pub rule_id: String,
    /// Who put the entry in the list.
    pub origin: RuleOrigin,
    /// The entry's label (`browser.trackers` for tracker blocking), if it
    /// has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The kind of pause the entry fits.
    pub pause: PauseName,
    /// The URL pattern the entry fits, for a pattern entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// The number of hosts the entry fits (each with its subdomains), for a
    /// host-set entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hosts_count: Option<usize>,
    /// The hosts, sorted, for a host-set entry, when `include_hosts` asked
    /// for them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<String>>,
    /// The resource type the entry fits, or absent for any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    /// What the entry does with a request it decides.
    pub action: ActionSummary,
}

impl RuleSummary {
    /// The summary of `entry`, at `index` of its list; `agent` says whether
    /// `browser_intercept_add_rule` added it.
    fn of(index: usize, entry: Entry, agent: bool) -> Self {
        let mut pattern = None;
        let mut hosts_count = None;
        let mut hosts = None;
        match entry.url {
            UrlInfo::Glob(glob) => pattern = Some(glob),
            UrlInfo::Hosts { count, hosts: list } => {
                hosts_count = Some(count);
                hosts = list;
            }
        }
        let action = match entry.action {
            ActionKind::Continue => ActionSummary::Continue,
            ActionKind::Block(reason) => ActionSummary::Block {
                reason: reason.as_cdp_str().into(),
            },
            ActionKind::Respond { status } => ActionSummary::Respond { status },
            ActionKind::RewriteUrl(to) => ActionSummary::Redirect { to },
            ActionKind::ModifyRequest => ActionSummary::ModifyRequest,
            ActionKind::ModifyResponse => ActionSummary::ModifyResponse,
            ActionKind::Stream => ActionSummary::Stream,
        };
        Self {
            index,
            rule_id: entry.id.to_string(),
            origin: if agent {
                RuleOrigin::Agent
            } else {
                RuleOrigin::Browser
            },
            label: entry.label,
            pause: PauseName::of(entry.pause),
            pattern,
            hosts_count,
            hosts,
            resource_type: entry.resource_type.map(|t| t.as_cdp_str().into()),
            action,
        }
    }
}

/// The current tab's proxy credentials.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ProxyAuthSummary {
    /// Whether the tab has proxy credentials (from `browser_open`'s
    /// `proxy`).
    pub configured: bool,
    /// Which requests Chrome pauses for them: `every_request`, or
    /// `documents_only`. Absent without credentials.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause: Option<AuthPauseName>,
}

/// Which requests Chrome pauses for the proxy credentials.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthPauseName {
    /// Every request: every proxy challenge gets the credentials.
    EveryRequest,
    /// Only documents: a challenge on another request's connection is
    /// missed, unless an entry pauses that request.
    DocumentsOnly,
}

/// A matcher for `hosts`, which must be bare host names: a rule for no
/// host, or for a name no URL has, would fit nothing while pausing every
/// request (Chrome can't be given a host set).
fn host_matcher(hosts: Vec<String>) -> Result<HostMatcher, ErrorData> {
    let bad = |h: &String| {
        let h = h.trim();
        h.is_empty()
            || h.contains(|c: char| {
                matches!(c, '/' | ':' | '*' | '?' | '#' | '@') || c.is_whitespace()
            })
    };
    if let Some(h) = hosts.iter().find(|h| bad(h)) {
        return Err(ErrorData::invalid_params(
            format!("`hosts` takes bare host names (`ads.example`), not `{h}`"),
            None,
        ));
    }
    if hosts.is_empty() {
        return Err(ErrorData::invalid_params("`hosts` is empty", None));
    }
    Ok(HostMatcher::new(hosts))
}

/// The id `id` (a `rule_id` string) names, or `RuleNotFound`.
fn rule_id(id: &str) -> Result<RuleId, ErrorData> {
    id.parse()
        .map_err(|_| map_error(McpServerError::RuleNotFound(id.into())))
}

/// `e` in the MCP error wire format; an id naming no entry is
/// `RuleNotFound`.
fn intercept_err(e: InterceptionError) -> ErrorData {
    match e {
        InterceptionError::NoSuchRule(id) => {
            map_error(McpServerError::RuleNotFound(id.to_string()))
        }
        e => map_error(McpServerError::from(ZendriverError::from(e))),
    }
}

/// `tab`'s list, read now, with the hosts of host sets if `include_hosts`;
/// `s` says which entries the agent added.
async fn listed(
    s: &SessionState,
    tab: &zendriver::Tab,
    include_hosts: bool,
) -> Result<ListRulesOutput, ErrorData> {
    let snapshot = tab
        .interceptor()
        .snapshot(include_hosts)
        .await
        .map_err(intercept_err)?;
    let rules = snapshot
        .entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            let agent = s.rules.contains_key(&entry.id);
            RuleSummary::of(index, entry, agent)
        })
        .collect();
    let pause = snapshot.proxy_auth.map(|p| match p {
        AuthPause::EveryRequest => AuthPauseName::EveryRequest,
        AuthPause::DocumentsOnly => AuthPauseName::DocumentsOnly,
    });
    let proxy_auth = ProxyAuthSummary {
        configured: pause.is_some(),
        pause,
    };
    Ok(ListRulesOutput { rules, proxy_auth })
}

/// The current tab's list, as [`listed`] reads it, or empty without a
/// current tab (no browser, or no tab, has no entries).
async fn listed_if_open(
    s: &SessionState,
    include_hosts: bool,
) -> Result<ListRulesOutput, ErrorData> {
    let Ok(tab) = current_tab(s).await else {
        let proxy_auth = ProxyAuthSummary {
            configured: false,
            pause: None,
        };
        return Ok(ListRulesOutput {
            rules: Vec::new(),
            proxy_auth,
        });
    };
    listed(s, &tab, include_hosts).await
}

/// Output of `browser_intercept_move_rule`, `_remove_rule` and
/// `_clear_rules`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RulesOutput {
    /// The current tab's list, read after the change, as
    /// `browser_intercept_list_rules` reports it.
    pub rules: Vec<RuleSummary>,
}

// ---------- browser_intercept_add_rule -----------------------------------

/// Input for `browser_intercept_add_rule`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddRuleInput {
    /// URL pattern (CDP wildcard syntax: `*` is any run of characters, `?`
    /// one character or none, `\` escapes), matched against the full
    /// request URL. Give exactly one of `pattern` and `hosts`.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Host names (`ads.example`, without scheme, port, path or wildcard):
    /// the rule fits a request whose host is one of these or a subdomain of
    /// one. Give exactly one of `pattern` and `hosts`.
    #[serde(default)]
    pub hosts: Option<Vec<String>>,
    /// The resource type the rule fits. Omit for any.
    #[serde(default)]
    pub resource_type: Option<ResourceTypeName>,
    /// The kind of pause the rule decides. Omit for `response_headers` with
    /// `modify_response`, `request` with the other actions.
    #[serde(default)]
    pub pause: Option<PauseName>,
    /// A name for the rule in `browser_intercept_list_rules` and logs.
    #[serde(default)]
    pub label: Option<String>,
    /// Action to take on matches.
    pub action: InterceptAction,
    /// Where the rule goes in the current tab's list: `last` unless this
    /// rule must take requests from an earlier entry. `first` is ahead of
    /// `browser.trackers`, so a `first` rule that fits a tracker's request
    /// decides it; `continue` or `modify_request` there sends the tracker
    /// request.
    pub position: PositionInput,
}

impl AddRuleInput {
    /// The rule this describes, and where it goes.
    fn into_rule(self) -> Result<(Rule, Position), ErrorData> {
        let at = self.position.position()?;
        let url = match (self.pattern, self.hosts) {
            (Some(glob), None) => Match::url(glob).map_err(intercept_err)?,
            (None, Some(hosts)) => Match::hosts(Arc::new(host_matcher(hosts)?)),
            _ => {
                return Err(ErrorData::invalid_params(
                    "give exactly one of `pattern` and `hosts`",
                    None,
                ));
            }
        };
        let kind = self.action.kind();
        let pause = match self.pause {
            Some(pause) => pause,
            None if kind == "modify_response" => PauseName::ResponseHeaders,
            None => PauseName::Request,
        };
        let mut when = url.at(pause.pause_kind());
        if let Some(t) = self.resource_type {
            when = when.resource(t.resource_type());
        }
        let rule = Rule::new(when, self.action.into_action());
        let mut rule = rule.map_err(|e| match e {
            InterceptionError::MismatchedPause { .. } => ErrorData::invalid_params(
                format!(
                    "action `{kind}` can't decide a `{}` pause: `redirect` and `modify_request` need `request`, `modify_response` needs `response_headers`",
                    pause.name()
                ),
                None,
            ),
            e => intercept_err(e),
        })?;
        if let Some(label) = self.label {
            rule = rule.label(label);
        }
        Ok((rule, at))
    }
}

/// Output of `browser_intercept_add_rule`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct AddRuleOutput {
    /// The new rule's id, for `position`, `browser_intercept_move_rule` and
    /// `browser_intercept_remove_rule`.
    pub rule_id: String,
    /// The current tab's list, read after the change, as
    /// `browser_intercept_list_rules` reports it.
    pub rules: Vec<RuleSummary>,
}

/// Insert one rule into the current tab's list, in force when this
/// returns, and keep its handle.
pub async fn add_rule(
    state: Arc<Mutex<SessionState>>,
    input: AddRuleInput,
) -> Result<AddRuleOutput, ErrorData> {
    let mut s = state.lock().await;
    let tab = current_tab(&s).await?;
    // So that `browser_tab_close` can drop the rule with its tab: see
    // `state::InterceptRuleHandle::tab_id`.
    let tab_id = tab.target_id().to_string();
    let (rule, at) = input.into_rule()?;
    let handle = tab
        .interceptor()
        .insert(vec![rule], at)
        .await
        .map_err(intercept_err)?;
    let &[id] = handle.ids() else {
        return Err(ErrorData::internal_error(
            "inserting one rule gave other than one id",
            None,
        ));
    };
    s.rules.insert(id, InterceptRuleHandle { handle, tab_id });
    let rules = listed(&s, &tab, false).await?.rules;
    Ok(AddRuleOutput {
        rule_id: id.to_string(),
        rules,
    })
}

/// Snapshot of a `modify_request` rule's overrides, captured once at
/// `add_rule` time and shared (via `Arc`) across every matching request.
struct ModifyRequestOverlay {
    headers: BTreeMap<String, String>,
    url: Option<String>,
    method: Option<String>,
    post_data: Option<String>,
}

impl ModifyRequestOverlay {
    /// Build the [`RequestOverrides`] payload for a `modify_request` rule.
    ///
    /// CDP's `Fetch.continueRequest.headers` is a *full replacement* — there
    /// is no merge mode — so we rebuild the header list from the request as
    /// Chrome reported it and overlay the caller's entries by
    /// case-insensitive name match. Case-insensitive: HTTP header names are
    /// case-insensitive per RFC 7230, and Chrome's emission casing isn't
    /// stable across versions. `url` / `method` / `post_data` are passed
    /// straight through — `None` leaves Chrome's original untouched.
    fn build(&self, original_headers: &[(String, String)]) -> RequestOverrides {
        RequestOverrides {
            url: self.url.clone(),
            method: self.method.clone(),
            headers: Some(merge_header_list(original_headers, &self.headers)),
            post_data: self.post_data.clone().map(String::into_bytes),
        }
    }
}

/// Rebuild a full header list from `original`, dropping entries the `overlay`
/// replaces (case-insensitive) and appending the overlay entries. Shared by
/// `modify_request` (→ [`RequestOverrides`]) and `modify_response`
/// (→ [`ResponseOverrides`]) because CDP's continue-* header fields are both
/// full replacements with no merge mode.
fn merge_header_list(
    original: &[(String, String)],
    overlay: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(original.len() + overlay.len());
    let overlay_keys_lower: std::collections::HashSet<String> =
        overlay.keys().map(|k| k.to_ascii_lowercase()).collect();
    for (k, v) in original {
        if !overlay_keys_lower.contains(&k.to_ascii_lowercase()) {
            out.push((k.clone(), v.clone()));
        }
    }
    for (k, v) in overlay {
        out.push((k.clone(), v.clone()));
    }
    out
}

// ---------- browser_intercept_move_rule ----------------------------------

/// Input for `browser_intercept_move_rule`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoveRuleInput {
    /// The id of the entry to move, from the current tab's list.
    pub rule_id: String,
    /// Where it goes, in the list as it is without it.
    pub position: PositionInput,
}

/// Move an entry of the current tab's list, and return once Chrome has
/// applied the change.
pub async fn move_rule(
    state: Arc<Mutex<SessionState>>,
    input: MoveRuleInput,
) -> Result<RulesOutput, ErrorData> {
    let s = state.lock().await;
    let tab = current_tab(&s).await?;
    let id = rule_id(&input.rule_id)?;
    let at = input.position.position()?;
    tab.interceptor()
        .move_rule(id, at)
        .await
        .map_err(intercept_err)?;
    let rules = listed(&s, &tab, false).await?.rules;
    Ok(RulesOutput { rules })
}

// ---------- browser_intercept_remove_rule --------------------------------

/// Input for `browser_intercept_remove_rule`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoveRuleInput {
    /// The id of the entry to remove: one `browser_intercept_add_rule`
    /// returned (on any tab), or one of the current tab's list.
    pub rule_id: String,
}

/// Remove an entry, and return once Chrome has applied the change, so a
/// request sent afterwards no longer meets it.
///
/// A rule the agent added is removed even if Chrome refuses the change
/// (its tab closed, say), so that only gets logged.
pub async fn remove_rule(
    state: Arc<Mutex<SessionState>>,
    input: RemoveRuleInput,
) -> Result<RulesOutput, ErrorData> {
    let mut s = state.lock().await;
    let id = rule_id(&input.rule_id)?;
    let removed = match s.rules.remove(&id) {
        Some(rule) => rule.handle.stop().await,
        None => {
            // Without a current tab, no list has the entry.
            let not_found = || map_error(McpServerError::RuleNotFound(input.rule_id.clone()));
            let tab = current_tab(&s).await.map_err(|_| not_found())?;
            tab.interceptor().remove(id).await
        }
    };
    match removed {
        Err(InterceptionError::NoSuchRule(_)) => {
            return Err(map_error(McpServerError::RuleNotFound(input.rule_id)));
        }
        // The entry is out of the list anyway.
        Err(e) => {
            tracing::debug!(error = %e, "browser_intercept_remove_rule: Chrome refused the change")
        }
        Ok(()) => {}
    }
    let rules = listed_if_open(&s, false).await?.rules;
    Ok(RulesOutput { rules })
}

// ---------- browser_intercept_list_rules ---------------------------------

/// Input for `browser_intercept_list_rules`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListRulesInput {
    /// List the hosts of host-set entries (`browser.trackers` included),
    /// not only their number. Defaults to `false`.
    #[serde(default)]
    pub include_hosts: bool,
}

/// Output of `browser_intercept_list_rules`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ListRulesOutput {
    /// The current tab's entries, in the order a paused request walks
    /// them. Empty without a current tab.
    pub rules: Vec<RuleSummary>,
    /// The current tab's proxy credentials, which are outside the list.
    pub proxy_auth: ProxyAuthSummary,
}

/// The current tab's list. Empty without a browser or a current tab (never
/// an error: no entries is a valid steady state).
pub async fn list_rules(
    state: Arc<Mutex<SessionState>>,
    input: ListRulesInput,
) -> Result<ListRulesOutput, ErrorData> {
    let s = state.lock().await;
    listed_if_open(&s, input.include_hosts).await
}

// ---------- browser_intercept_clear_rules --------------------------------

/// Remove every rule the agent added (on every tab), and return once
/// Chrome has applied the change. The browser's entries stay.
///
/// Every rule is removed even if Chrome refuses a change (its tab closed,
/// say), so that only gets logged: like having no rules, it is no error.
pub async fn clear_rules(
    state: Arc<Mutex<SessionState>>,
    _: crate::tools::common::EmptyInput,
) -> Result<RulesOutput, ErrorData> {
    let mut s = state.lock().await;
    for (_, rule) in s.rules.drain() {
        if let Err(e) = rule.handle.stop().await {
            tracing::debug!(error = %e, "browser_intercept_clear_rules: Chrome refused a change");
        }
    }
    let rules = listed_if_open(&s, false).await?.rules;
    Ok(RulesOutput { rules })
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    //! No-browser unit coverage.
    //!
    //! The browser-touching paths need a live Chrome; they are exercised in
    //! `tests/interception_rules.rs`. Here: the empty list without a
    //! browser, an unknown id, the input checks, and the overlays.

    use super::*;
    use crate::tools::common::EmptyInput;

    /// An `add_rule` input for `pattern` with `action`, at `last`.
    fn add(
        pattern: Option<&str>,
        hosts: Option<Vec<String>>,
        action: InterceptAction,
    ) -> AddRuleInput {
        AddRuleInput {
            pattern: pattern.map(Into::into),
            hosts,
            resource_type: None,
            pause: None,
            label: None,
            action,
            position: PositionInput::Last,
        }
    }

    #[tokio::test]
    async fn without_a_browser_the_list_is_empty_and_clearing_is_no_error() {
        let state = Arc::new(Mutex::new(SessionState::new()));
        let listed = list_rules(state.clone(), ListRulesInput::default())
            .await
            .unwrap();
        assert!(listed.rules.is_empty());
        assert!(!listed.proxy_auth.configured);
        let cleared = clear_rules(state, EmptyInput {}).await.unwrap();
        assert!(cleared.rules.is_empty());
    }

    /// An id that isn't one, and one that names no entry, are both
    /// `RuleNotFound`, with or without a browser.
    #[tokio::test]
    async fn remove_unknown_rule_surfaces_rule_not_found() {
        for id in ["nope", "123456789"] {
            let state = Arc::new(Mutex::new(SessionState::new()));
            let input = RemoveRuleInput { rule_id: id.into() };
            let err = remove_rule(state, input)
                .await
                .expect_err("expected RuleNotFound");
            assert!(err.message.contains(id), "{}", err.message);
            let data = err.data.as_ref().expect("data populated");
            assert_eq!(data["suggested_next"], "browser_intercept_list_rules");
        }
    }

    #[tokio::test]
    async fn add_rule_with_no_browser_errors() {
        let state = Arc::new(Mutex::new(SessionState::new()));
        let err = add_rule(state, add(Some("*"), None, InterceptAction::Block))
            .await
            .expect_err("expected BrowserNotOpen");
        assert!(err.message.contains("Browser not open"));
    }

    /// A rule needs exactly one of `pattern` and `hosts`, and an action its
    /// pause can take.
    #[test]
    fn a_rule_needs_one_url_match_and_a_fitting_pause() {
        let both = add(
            Some("*"),
            Some(vec!["a.test".into()]),
            InterceptAction::Block,
        );
        assert!(both.into_rule().is_err());
        assert!(add(None, None, InterceptAction::Block).into_rule().is_err());
        let hosts = add(None, Some(vec!["a.test".into()]), InterceptAction::Continue);
        assert!(hosts.into_rule().is_ok());

        for hosts in [
            vec![],
            vec!["*.ads.test".into()],
            vec!["https://ads.test/".into()],
        ] {
            assert!(
                add(None, Some(hosts), InterceptAction::Block)
                    .into_rule()
                    .is_err()
            );
        }

        let mut redirect = add(
            Some("*"),
            None,
            InterceptAction::Redirect {
                to: "https://b.test/".into(),
            },
        );
        redirect.pause = Some(PauseName::ResponseHeaders);
        let err = redirect.into_rule().unwrap_err();
        assert!(err.message.contains("`redirect`"), "{}", err.message);
        assert!(
            err.message.contains("`response_headers`"),
            "{}",
            err.message
        );

        let modify = InterceptAction::ModifyResponse {
            status: Some(203),
            headers: BTreeMap::new(),
        };
        let (rule, _) = add(Some("*"), None, modify).into_rule().unwrap();
        assert!(format!("{rule:?}").contains("ResponseHeaders"), "{rule:?}");
    }

    #[test]
    fn a_position_names_entries_by_rule_id() {
        let after: PositionInput =
            serde_json::from_value(serde_json::json!({ "after": "7" })).unwrap();
        assert!(matches!(after.position().unwrap(), Position::After(id) if id.to_string() == "7"));
        let last: PositionInput = serde_json::from_value(serde_json::json!("last")).unwrap();
        assert!(matches!(last.position().unwrap(), Position::Last));
        let bad = PositionInput::Before("x".into()).position().unwrap_err();
        assert!(bad.message.contains("`x`"), "{}", bad.message);
    }

    #[test]
    fn merge_headers_overlays_case_insensitively_and_drops_originals() {
        let original = vec![
            ("Host".to_string(), "example.com".to_string()),
            ("User-Agent".to_string(), "old".to_string()),
            ("Accept".to_string(), "*/*".to_string()),
        ];
        let mut overlay = BTreeMap::new();
        overlay.insert("user-agent".to_string(), "new".to_string());
        overlay.insert("X-Marker".to_string(), "yes".to_string());

        let snapshot = ModifyRequestOverlay {
            headers: overlay,
            url: None,
            method: None,
            post_data: None,
        };
        let ov = snapshot.build(&original);
        assert!(ov.url.is_none());
        assert!(ov.method.is_none());
        assert!(ov.post_data.is_none());
        let headers = ov.headers.expect("headers populated");

        // Original "User-Agent" gone (case-insens), replaced by overlay's
        // "user-agent: new". Host + Accept survive. Overlay's X-Marker is
        // appended.
        let names_lower: Vec<String> = headers
            .iter()
            .map(|(k, _)| k.to_ascii_lowercase())
            .collect();
        assert!(names_lower.contains(&"host".into()));
        assert!(names_lower.contains(&"accept".into()));
        assert!(names_lower.contains(&"user-agent".into()));
        assert!(names_lower.contains(&"x-marker".into()));
        // Exactly one "user-agent" (no dupes).
        assert_eq!(names_lower.iter().filter(|n| *n == "user-agent").count(), 1);
        // Replaced value is the overlay's.
        let ua = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
            .map(|(_, v)| v.as_str())
            .expect("user-agent present");
        assert_eq!(ua, "new");
    }

    /// A `modify_request` rule with `url` / `method` / `post_data` set must
    /// produce a [`RequestOverrides`] carrying all three verbatim (`post_data`
    /// UTF-8-encoded into bytes), alongside the usual header merge — the DTO
    /// fields this test exercises used to be dropped on the floor ("reserved
    /// for a follow-up").
    #[test]
    fn modify_request_overlay_threads_url_method_post_data_into_overrides() {
        let original = vec![("Host".to_string(), "example.com".to_string())];
        let mut headers = BTreeMap::new();
        headers.insert("x-injected".to_string(), "1".to_string());

        let snapshot = ModifyRequestOverlay {
            headers,
            url: Some("https://example.com/replaced".to_string()),
            method: Some("POST".to_string()),
            post_data: Some(r#"{"a":1}"#.to_string()),
        };
        let ov = snapshot.build(&original);

        assert_eq!(ov.url.as_deref(), Some("https://example.com/replaced"));
        assert_eq!(ov.method.as_deref(), Some("POST"));
        assert_eq!(ov.post_data.as_deref(), Some(br#"{"a":1}"#.as_slice()));
        let headers = ov.headers.expect("headers populated");
        assert!(
            headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("x-injected") && v == "1")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("host") && v == "example.com")
        );
    }

    /// When `url` / `method` / `post_data` are all `None` (the pre-existing
    /// headers-only shape), the resulting `RequestOverrides` must leave them
    /// `None` too — no accidental empty-string overrides.
    #[test]
    fn modify_request_overlay_omits_absent_scalar_overrides() {
        let snapshot = ModifyRequestOverlay {
            headers: BTreeMap::new(),
            url: None,
            method: None,
            post_data: None,
        };
        let ov = snapshot.build(&[]);
        assert!(ov.url.is_none());
        assert!(ov.method.is_none());
        assert!(ov.post_data.is_none());
    }
}
