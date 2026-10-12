use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::timestamp;
use futures_util::future::join_all;
use p1_auth::{CredentialKind, Locations};
use p1_provider_http::Transport;
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;

use crate::{
    Credits, ExtraUsage, FailKind, Probe, RouteUsage, Snapshot, UsageRoute, Window, WindowKind,
};

type ProbeFuture<'a> = Pin<Box<dyn Future<Output = RouteUsage> + Send + 'a>>;

/// `user-agent` for the probes that ask for one: the vendor asks for a caller version, and it
/// must not be a module's business (a module cannot know which p1 calls it).
const USER_AGENT: &str = concat!("p1/", env!("CARGO_PKG_VERSION"));

/// One installed usage probe (ADR-0214): a module of the `usage-probe` class, as the ledger
/// calls it. It never receives a credential: [`describe`](Self::describe) says where its
/// endpoint is, the ledger sends the request with the credential attached natively, and
/// [`decode`](Self::decode) reads a 2xx response's body. Both answers are the JSON texts
/// `docs/design/modules/usage-probe.md` defines, and the ledger checks both.
pub trait ProbeModule: Send + Sync {
    /// The endpoint, as the JSON text the module wrote.
    fn describe(&self) -> Result<String, String>;

    /// What a 2xx response `body` says, as the JSON text the module wrote, or why it says
    /// nothing readable.
    fn decode(&self, body: &[u8]) -> Result<String, String>;

    /// The https origins the module's verified manifest lets its request go to, as the owner
    /// confirmed them when it was installed.
    fn token_origins(&self) -> &[String];
}

/// The usage probes a host has installed, by the name accounts give them (`usage = "…"`,
/// ADR-0139) and credential methods name for their logins (ADR-0211). The host composes them
/// explicitly; nothing registers itself.
pub trait ProbeModules: Send + Sync {
    /// Every installed probe's name, sorted.
    fn names(&self) -> Vec<String>;

    /// The probe named `name`, or `None` when none is installed under that name.
    fn module(&self, name: &str) -> Result<Option<Arc<dyn ProbeModule>>, String>;
}

/// No usage probe at all: every route probes as unsupported without a request.
pub struct NoProbes;

impl ProbeModules for NoProbes {
    fn names(&self) -> Vec<String> {
        Vec::new()
    }

    fn module(&self, _name: &str) -> Result<Option<Arc<dyn ProbeModule>>, String> {
        Ok(None)
    }
}

/// A probe module's description, closed: anything else is refused.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Description {
    url: String,
    method: String,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default)]
    account_id_header: Option<String>,
    #[serde(default)]
    user_agent: bool,
    #[serde(default)]
    borrowed: bool,
}

/// The headers a probe may not name: the ledger sets the credential, the client identity and
/// the connection itself.
const RESERVED_HEADERS: [&str; 8] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "user-agent",
];

/// The endpoint one route's probe reaches: its module's checked description.
struct Endpoint {
    module: Arc<dyn ProbeModule>,
    url: String,
    origin: String,
    method: String,
    headers: Vec<(String, String)>,
    account_id_header: Option<String>,
    user_agent: bool,
    borrowed: bool,
}

impl Endpoint {
    /// The endpoint `module` describes, checked: an https url at an origin the module's manifest
    /// declares, and header names and values a request can carry and that are not the ledger's.
    fn of(module: Arc<dyn ProbeModule>) -> Result<Self, String> {
        let text = module.describe()?;
        let description: Description = serde_json::from_str(&text)
            .map_err(|error| format!("its description is not one: {error}"))?;
        let origin = https_origin(&description.url)
            .ok_or("its endpoint is not an https url without credentials")?;
        if !module.token_origins().contains(&origin) {
            return Err(format!(
                "its endpoint origin {origin} is not one of the origins its manifest declares"
            ));
        }
        let header_name = |name: &str| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                && !RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str())
        };
        for (name, value) in &description.headers {
            if !header_name(name) || !value.bytes().all(|b| (32..127).contains(&b)) {
                return Err(format!("its header {name:?} is not one a probe may send"));
            }
        }
        if let Some(name) = &description.account_id_header
            && (!header_name(name)
                || description
                    .headers
                    .iter()
                    .any(|(other, _)| other.eq_ignore_ascii_case(name)))
        {
            return Err(format!(
                "its account id header {name:?} is not one a probe may send"
            ));
        }
        Ok(Self {
            module,
            url: description.url,
            origin,
            method: description.method,
            headers: description.headers,
            account_id_header: description.account_id_header,
            user_agent: description.user_agent,
            borrowed: description.borrowed,
        })
    }

    /// Whether the probe takes an API key, rather than an OAuth credential a method refreshes.
    fn api_key(&self) -> bool {
        self.method == CredentialKind::API_KEY.name()
    }

    fn failure(&self, status: u16, bytes: &[u8]) -> Probe {
        // An API-key route that refuses the key is a credential problem, not a transport
        // one. The OAuth probes keep reporting the status they always did.
        if self.api_key() && matches!(status, 401 | 402) {
            return Probe::Failed {
                kind: FailKind::Credential,
                detail: format!("HTTP {status}"),
            };
        }
        http_failure(status, bytes)
    }
}

/// `https://host[:port]` of an https `url` that names no user, or `None`.
fn https_origin(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    (!authority.is_empty()
        && !authority.contains('@')
        && authority
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-:".contains(&b)))
    .then(|| format!("https://{authority}"))
}

/// Why a route has no probe: it sends no credential, or no installed probe serves it.
fn no_probe(route: &UsageRoute) -> Probe {
    // A route that sends no credential has no endpoint p1 could probe:
    // the egress proxy injects the credential (issue #134).
    let reason = if route.spec.kind == CredentialKind::NONE {
        "p1 sends no credential on this route (kind \"none\"), so it has none to \
         present to a usage endpoint"
    } else {
        "no usage endpoint known for this route"
    };
    Probe::Unsupported {
        reason: reason.into(),
    }
}

/// The endpoint of `route`'s probe, `None` when it has none. An account names its probe
/// (ADR-0139 `usage`); without one, the probe its credential method's login answers
/// (ADR-0211). An API-key account gets a probe only from its `usage` name: guessing an endpoint
/// from its id would probe somebody else's service with the owner's credential, so the free
/// Zen and ClinePass accounts, which name none, stay `Unsupported` (docs/design/usage.md). A
/// route that sends no credential (issue #134) cannot be probed: the egress proxy injects the
/// credential, so p1 has none to present. A probe still takes only the credential method its
/// description names.
fn endpoint(
    route: &UsageRoute,
    locations: &Locations,
    probes: &dyn ProbeModules,
) -> Result<Option<Endpoint>, String> {
    let name = match &route.probe {
        Some(name) => name.clone(),
        None => match locations
            .method(&route.spec.kind)
            .ok()
            .and_then(|method| method.usage_probe().map(str::to_owned))
        {
            Some(name) => name,
            None => return Ok(None),
        },
    };
    let unusable = |reason: String| format!("usage probe `{name}` cannot be used: {reason}");
    let Some(module) = probes.module(&name).map_err(unusable)? else {
        return Err(format!("usage probe `{name}` is not installed"));
    };
    let endpoint = Endpoint::of(module).map_err(unusable)?;
    Ok((endpoint.method == route.spec.kind.name()).then_some(endpoint))
}

/// Injectable probe boundary: each route finishes independently of the others.
pub trait UsageProbe: Send + Sync {
    fn probe<'a>(
        &'a self,
        route: &'a UsageRoute,
        locations: &'a Locations,
        transport: Arc<dyn Transport>,
    ) -> ProbeFuture<'a>;
}

/// The probe that reaches each route's usage endpoint over HTTPS, through the installed
/// [`ProbeModules`].
pub struct HttpProbe {
    probes: Arc<dyn ProbeModules>,
}

impl HttpProbe {
    pub fn new(probes: Arc<dyn ProbeModules>) -> Self {
        Self { probes }
    }
}

impl UsageProbe for HttpProbe {
    fn probe<'a>(
        &'a self,
        route: &'a UsageRoute,
        locations: &'a Locations,
        transport: Arc<dyn Transport>,
    ) -> ProbeFuture<'a> {
        Box::pin(async move {
            let mut result = RouteUsage::empty(route);
            let endpoint = match endpoint(route, locations, self.probes.as_ref()) {
                Ok(Some(endpoint)) => endpoint,
                Ok(None) => {
                    result.probe = no_probe(route);
                    return result;
                }
                Err(reason) => {
                    result.probe = Probe::Unsupported {
                        reason: masked_detail(&reason),
                    };
                    return result;
                }
            };
            let store_origin = match check_probe_origin(route, &endpoint, locations) {
                Ok(store_origin) => store_origin,
                Err(reason) => {
                    result.probe = Probe::Unsupported {
                        reason: masked_detail(&reason),
                    };
                    return result;
                }
            };
            let source = p1_auth::resolve_with_store_origin(
                route.store_id(),
                &route.spec,
                transport,
                locations,
                Some(&endpoint.origin),
                store_origin,
            );
            let credential = match source.access().await {
                Ok(value) => value,
                Err(error) => {
                    // A credential error names paths and route ids, never a value by
                    // contract; a path or id can still carry one, so the detail is masked.
                    result.probe = Probe::Failed {
                        kind: FailKind::Credential,
                        detail: masked_detail(&error.to_string()),
                    };
                    return result;
                }
            };
            // Environment keys also need approval rechecked after acquisition.
            if let Err(reason) = check_probe_origin(route, &endpoint, locations) {
                result.probe = Probe::Unsupported {
                    reason: masked_detail(&reason),
                };
                return result;
            }
            if endpoint.account_id_header.is_some() && credential.account_id.is_none() {
                result.probe = Probe::Failed {
                    kind: FailKind::Credential,
                    detail: "account id unavailable".into(),
                };
                return result;
            }
            let client = match reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .build()
            {
                Ok(client) => client,
                Err(_) => {
                    result.probe = Probe::Failed {
                        kind: FailKind::Network,
                        detail: "client initialization failed".into(),
                    };
                    return result;
                }
            };
            // Secret material is used only in request construction; no response or error carries
            // it onward, and the module that described the endpoint never sees it.
            let mut request = client.get(&endpoint.url).bearer_auth(&credential.bearer);
            for (name, value) in &endpoint.headers {
                request = request.header(name, value);
            }
            if let Some(name) = &endpoint.account_id_header {
                request =
                    request.header(name, credential.account_id.as_deref().unwrap_or_default());
            }
            if endpoint.user_agent {
                request = request.header("user-agent", USER_AGENT);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(_) => {
                    result.probe = Probe::Failed {
                        kind: FailKind::Network,
                        detail: "request failed".into(),
                    };
                    return result;
                }
            };
            let status = response.status();
            let bytes = match response.bytes().await {
                Ok(bytes) => bytes,
                Err(_) => {
                    result.probe = Probe::Failed {
                        kind: FailKind::Network,
                        detail: "response read failed".into(),
                    };
                    return result;
                }
            };
            let sent = Sent::of(&credential.bearer);
            if !status.is_success() {
                result.probe = endpoint.failure(status.as_u16(), &bytes);
                return sent.forget_echoes(result);
            }
            sent.forget_echoes(decode_usage(route, endpoint.module.as_ref(), &bytes))
        })
    }
}

/// `true` requires an atomic store-origin check at credential acquisition. Borrowed
/// kinds use only the origins their probe declares safe for a borrowed login; API/store-only
/// ids need recorded approval for the *probe* origin, not their chat endpoint (ADR-0110).
fn check_probe_origin(
    route: &UsageRoute,
    endpoint: &Endpoint,
    locations: &Locations,
) -> Result<bool, String> {
    let origin = &endpoint.origin;
    if let Some(origins) = &route.compiled_origins {
        return if origins.iter().any(|approved| approved == origin) {
            Ok(false)
        } else {
            Err(format!(
                "route `{}` usage endpoint origin {origin} is not approved by its compiled credential anchor",
                route.route_id
            ))
        };
    }
    let borrowed = !route.spec.store_only
        && locations
            .method(&route.spec.kind)
            .is_ok_and(|method| !method.borrowed(&route.spec).is_empty());
    // A probe whose origin a borrowed credential may reach says so (`borrowed`); the owner
    // confirmed that origin when the probe was installed (ADR-0214). Kimi's does not: no Kimi
    // login another tool keeps is trusted (ADR-0061), so a borrowed Kimi credential never
    // authorizes its probe.
    if borrowed && !endpoint.borrowed {
        return Err(format!(
            "route `{}` usage endpoint origin {origin} is not approved for a borrowed {} credential",
            route.route_id,
            route.spec.kind.name()
        ));
    }
    let recorded = p1_auth::store::endpoint_origins(route.store_id(), locations)?;
    if borrowed
        && !endpoint.api_key()
        && (recorded.is_empty() || recorded.iter().any(|value| value == origin))
    {
        return Ok(false);
    }
    if recorded.iter().any(|value| value == origin) {
        return Ok(true);
    }
    Err(format!(
        "route `{}` usage endpoint origin {origin} is not approved; run `p1 login {} --trust-endpoint` for a route at this origin",
        route.route_id, route.route_id
    ))
}

fn http_failure(status: u16, bytes: &[u8]) -> Probe {
    let parsed: Option<Value> = serde_json::from_slice(bytes).ok();
    let code = parsed
        .as_ref()
        .and_then(|v| {
            v.pointer("/error/type")
                .or_else(|| v.pointer("/error/code"))
                .or_else(|| v.get("type"))
                .or_else(|| v.get("code"))
        })
        .and_then(Value::as_str)
        .and_then(error_code);
    Probe::Failed {
        kind: FailKind::Http(status),
        detail: match code {
            Some(code) => format!("HTTP {status} · {code}"),
            None => format!("HTTP {status}"),
        },
    }
}

/// A probe module's answer for a 2xx response, closed: anything else is refused.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    windows: Vec<AnswerWindow>,
    #[serde(default)]
    credits: Option<AnswerCredits>,
    #[serde(default)]
    extra_usage: Option<AnswerExtraUsage>,
    /// The vendor refused the credential in a 2xx body (GLM's `success: false`).
    #[serde(default)]
    credential_refused: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerWindow {
    kind: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    used_percent: Option<f64>,
    #[serde(default)]
    resets_at: Option<String>,
    #[serde(default)]
    limit_reached: bool,
    #[serde(default)]
    detail: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerCredits {
    #[serde(default)]
    balance: Option<f64>,
    #[serde(default)]
    reset_credits: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerExtraUsage {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    used: Option<f64>,
    #[serde(default)]
    limit: Option<f64>,
    #[serde(default)]
    currency: Option<String>,
}

/// What the 2xx response `body` of `route`'s probe says, as the ledger keeps it: `module`
/// decodes it, and every string of the answer keeps only its own shape (a label, a reset
/// instant, a currency, a count) before it is shown (review #484). An unreadable body, or an
/// answer that is not one, is a parse failure with nothing kept.
pub fn decode_usage(route: &UsageRoute, module: &dyn ProbeModule, body: &[u8]) -> RouteUsage {
    let mut out = RouteUsage::empty(route);
    let answer = module
        .decode(body)
        .ok()
        .and_then(|text| serde_json::from_str::<Answer>(&text).ok());
    let Some(answer) = answer else {
        out.probe = Probe::Failed {
            kind: FailKind::Parse,
            detail: "invalid usage response".into(),
        };
        return out;
    };
    if answer.credential_refused {
        out.probe = Probe::Failed {
            kind: FailKind::Credential,
            detail: "usage credential refused".into(),
        };
        return out;
    }
    let mut windows = Vec::with_capacity(answer.windows.len());
    for window in answer.windows {
        let kind = match window.kind.as_str() {
            "session" => WindowKind::Session,
            "weekly" => WindowKind::Weekly,
            "weekly-scoped" => WindowKind::WeeklyScoped,
            // An unknown kind is shown by name only when it looks like one.
            "other" => WindowKind::Other(
                window
                    .label
                    .as_deref()
                    .and_then(label)
                    .unwrap_or_else(|| "other".into()),
            ),
            _ => {
                out.probe = Probe::Failed {
                    kind: FailKind::Parse,
                    detail: "invalid usage response".into(),
                };
                return out;
            }
        };
        windows.push(Window {
            kind,
            scope: window.scope.as_deref().and_then(label),
            used_percent: window.used_percent,
            resets_at: window.resets_at.as_deref().and_then(reset_text),
            limit_reached: window.limit_reached,
            detail: window.detail.as_deref().and_then(detail),
        });
    }
    out.windows = windows;
    out.plan = answer.plan.as_deref().and_then(label);
    out.credits = answer.credits.map(|credits| Credits {
        balance: credits.balance,
        reset_credits: credits.reset_credits,
        used: None,
        limit: None,
        currency: None,
    });
    out.extra_usage = answer.extra_usage.map(|extra| ExtraUsage {
        enabled: extra.enabled,
        used: extra.used,
        limit: extra.limit,
        currency: extra.currency.as_deref().and_then(currency),
    });
    out
}

/// The longest vendor label kept (a plan, a scope's model name, a window kind).
const LABEL_LIMIT: usize = 40;

/// The longest window detail kept (`<remaining>/<limit> left`, each count at most 20 bytes).
const WINDOW_DETAIL_LIMIT: usize = 64;

/// The longest failure detail kept.
const DETAIL_LIMIT: usize = 240;

/// A vendor label the ledger shows (review #484): short, plain text, and not shaped like a
/// credential. Anything else is dropped rather than shown.
fn label(text: &str) -> Option<String> {
    shown(text, LABEL_LIMIT)
}

/// A window's detail text, under the label rule with room for two counts.
fn detail(text: &str) -> Option<String> {
    shown(text, WINDOW_DETAIL_LIMIT)
}

fn shown(text: &str, limit: usize) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()
        && text.len() <= limit
        && text.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(ch, ' ' | '_' | '-' | '.' | '(' | ')' | '/' | '+')
        })
        && !credential_shaped(text))
    .then(|| text.to_string())
}

/// An ISO 4217 currency code, upper-cased; anything else is not a currency.
fn currency(text: &str) -> Option<String> {
    let text = text.trim();
    (text.len() == 3 && text.bytes().all(|b| b.is_ascii_alphabetic()))
        .then(|| text.to_ascii_uppercase())
}

/// A reset instant as the vendor wrote it, kept only when it IS an RFC 3339 instant: digits
/// and the timestamp punctuation only, and parseable.
fn reset_text(text: &str) -> Option<String> {
    let text = text.trim();
    (text.len() <= 35
        && text
            .chars()
            .all(|ch| ch.is_ascii_digit() || matches!(ch, '-' | ':' | '.' | 'T' | 'Z' | '+'))
        && timestamp::parse(text).is_some())
    .then(|| text.to_string())
}

/// A vendor error code worth showing: the shared provider rule (token-shaped, bounded, no
/// `secret`/`password`/`sk-`), at most 40 bytes, and not shaped like a credential.
fn error_code(value: &str) -> Option<&str> {
    p1_provider_http::safe_code(value).filter(|code| code.len() <= 40 && !credential_shaped(code))
}

/// Whether `text` holds something shaped like a key or token: an `sk-` key, or a run of 16+
/// letters and digits without a separator that mixes letters with digits (hex keys, JWT
/// segments, random tokens), or 24+ that mixes cases. A word, an identifier in snake case or
/// a model name has neither.
fn credential_shaped(text: &str) -> bool {
    if text.to_ascii_lowercase().contains("sk-") {
        return true;
    }
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|run| {
            let digits = run.bytes().any(|b| b.is_ascii_digit());
            let letters = run.bytes().any(|b| b.is_ascii_alphabetic());
            let upper = run.bytes().any(|b| b.is_ascii_uppercase());
            let lower = run.bytes().any(|b| b.is_ascii_lowercase());
            (run.len() >= 16 && digits && letters) || (run.len() >= 24 && upper && lower)
        })
}

/// A failure detail that came from outside this crate (a credential error, a module's
/// reason), with every credential-shaped run masked and its length bounded.
fn masked_detail(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(DETAIL_LIMIT));
    for word in text.split_inclusive(char::is_whitespace) {
        let bare = word.trim_end();
        if credential_shaped(bare) {
            out.push_str(&format!("<redacted:{} chars>", bare.len()));
            out.push_str(&word[bare.len()..]);
        } else {
            out.push_str(word);
        }
    }
    if out.len() > DETAIL_LIMIT {
        let mut end = DETAIL_LIMIT;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push('…');
    }
    out
}

/// The credential one probe sent. Every string the response hands back is checked against
/// it before the snapshot keeps it: a string that carries it is dropped (review #484).
struct Sent<'a> {
    secret: &'a str,
}

impl<'a> Sent<'a> {
    fn of(secret: &'a str) -> Self {
        Self { secret }
    }

    /// Whether `text` holds the credential, or is a piece of it long enough to matter.
    fn echoed(&self, text: &str) -> bool {
        let secret = self.secret;
        !secret.is_empty() && (text.contains(secret) || (text.len() >= 8 && secret.contains(text)))
    }

    fn keep(&self, value: Option<String>) -> Option<String> {
        value.filter(|text| !self.echoed(text))
    }

    /// The usage with every response-derived string that echoes the credential removed.
    fn forget_echoes(&self, mut usage: RouteUsage) -> RouteUsage {
        usage.plan = self.keep(usage.plan.take());
        for window in &mut usage.windows {
            if let WindowKind::Other(name) = &window.kind
                && self.echoed(name)
            {
                window.kind = WindowKind::Other("other".into());
            }
            window.scope = self.keep(window.scope.take());
            window.resets_at = self.keep(window.resets_at.take());
            window.detail = self.keep(window.detail.take());
        }
        if let Some(credits) = &mut usage.credits {
            credits.currency = self.keep(credits.currency.take());
        }
        if let Some(extra) = &mut usage.extra_usage {
            extra.currency = self.keep(extra.currency.take());
        }
        if let Probe::Failed { kind, detail } = &mut usage.probe
            && self.echoed(detail)
        {
            *detail = match kind {
                FailKind::Http(status) => format!("HTTP {status}"),
                _ => "the response echoed the credential".into(),
            };
        }
        usage
    }
}

pub async fn snapshot(
    routes: &[UsageRoute],
    locations: &Locations,
    transport: Arc<dyn Transport>,
    probes: Arc<dyn ProbeModules>,
) -> Snapshot {
    snapshot_with(routes, locations, transport, &HttpProbe::new(probes)).await
}

pub async fn snapshot_with<P: UsageProbe + ?Sized>(
    routes: &[UsageRoute],
    locations: &Locations,
    transport: Arc<dyn Transport>,
    probe: &P,
) -> Snapshot {
    let taken_at = timestamp::format(OffsetDateTime::now_utc());
    let mut routes = join_all(
        routes
            .iter()
            .map(|route| probe.probe(route, locations, transport.clone())),
    )
    .await;
    // Fresh data is timestamped here; the host replaces this with the last good time when it
    // keeps a stale row (see the live watch loop).
    for route in &mut routes {
        if matches!(route.probe, Probe::Supported) {
            route.observed_at = Some(taken_at.clone());
        }
    }
    Snapshot { taken_at, routes }
}

/// The one non-route source the dashboard shows: OpenRouter credits. OpenRouter has no model
/// route in p1 and no quota windows; the host supplies the existing configured key (never
/// logged) and this makes one bounded read-only request, or an explicit failure.
pub async fn openrouter_credits(bearer: &str) -> RouteUsage {
    let mut out = RouteUsage::new(
        "openrouter-credits",
        "openrouter credits",
        "brain-tools openrouter.key",
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            out.probe = Probe::Failed {
                kind: FailKind::Network,
                detail: "client initialization failed".into(),
            };
            return out;
        }
    };
    let response = match client
        .get("https://openrouter.ai/api/v1/credits")
        .bearer_auth(bearer)
        .header("user-agent", USER_AGENT)
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => {
            out.probe = Probe::Failed {
                kind: FailKind::Network,
                detail: "request failed".into(),
            };
            return out;
        }
    };
    let status = response.status();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => {
            out.probe = Probe::Failed {
                kind: FailKind::Network,
                detail: "response read failed".into(),
            };
            return out;
        }
    };
    let sent = Sent::of(bearer);
    if !status.is_success() {
        // A refused key is `no access`; every other status keeps the HTTP kind.
        let refused = matches!(status.as_u16(), 401 | 402);
        out.probe = match http_failure(status.as_u16(), &bytes) {
            Probe::Failed { kind, detail } => Probe::Failed {
                kind: if refused { FailKind::Credential } else { kind },
                detail: if refused {
                    format!("HTTP {status}")
                } else {
                    detail
                },
            },
            other => other,
        };
        return sent.forget_echoes(out);
    }
    match parse_openrouter_credits(&bytes) {
        Some((used, limit, balance)) => {
            out.credits = Some(Credits {
                balance: Some(balance),
                reset_credits: None,
                used: Some(used),
                limit: Some(limit),
                currency: Some("USD".into()),
            });
        }
        None => {
            out.probe = Probe::Failed {
                kind: FailKind::Parse,
                detail: "invalid credits response".into(),
            };
        }
    }
    out
}

/// `{"data": {"total_credits", "total_usage"}}` as `(used, limit, balance)` in USD. The
/// cents are rounded once and the balance derived from them, so `used + balance == limit` in
/// the display and the three values never disagree by a cent.
fn parse_openrouter_credits(bytes: &[u8]) -> Option<(f64, f64, f64)> {
    let data: Value = serde_json::from_slice(bytes).ok()?;
    let data = data.get("data")?;
    let limit = round_cents(data.get("total_credits").and_then(Value::as_f64)?);
    let used = round_cents(
        data.get("total_usage")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    );
    Some((used, limit, round_cents(limit - used)))
}

fn round_cents(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A usage probe described by `describe` and answering every body with `answer`; the
    /// connectors' own decoders are checked in `connectors/subscriptions/tests`.
    struct Fake {
        describe: String,
        answer: Result<String, String>,
        origins: Vec<String>,
    }

    impl ProbeModule for Fake {
        fn describe(&self) -> Result<String, String> {
            Ok(self.describe.clone())
        }

        fn decode(&self, _body: &[u8]) -> Result<String, String> {
            self.answer.clone()
        }

        fn token_origins(&self) -> &[String] {
            &self.origins
        }
    }

    /// The description of the probe `name` the subscription connectors install, as their
    /// `usage-*` components describe it (ADR-0214).
    fn description(name: &str) -> Value {
        let (url, method, headers, account_id_header, user_agent, borrowed) = match name {
            "claude" => (
                "https://api.anthropic.com/api/oauth/usage",
                "claude-code-oauth",
                json!([["anthropic-beta", "oauth-2025-04-20"]]),
                None,
                false,
                true,
            ),
            "codex" => (
                "https://chatgpt.com/backend-api/wham/usage",
                "codex-oauth",
                json!([]),
                Some("ChatGPT-Account-Id"),
                false,
                true,
            ),
            "opencode-go" => (
                "https://opencode.ai/zen/go/v1/usage",
                "api-key",
                json!([["x-opencode-session", "p1-usage"]]),
                None,
                true,
                true,
            ),
            "kimi" => (
                "https://api.kimi.ai/coding/v1/usages",
                "api-key",
                json!([]),
                None,
                false,
                false,
            ),
            "glm" => (
                "https://api.z.ai/api/monitor/usage/quota/limit",
                "api-key",
                json!([]),
                None,
                false,
                true,
            ),
            other => panic!("no fixture probe {other}"),
        };
        json!({
            "url": url,
            "method": method,
            "headers": headers,
            "account_id_header": account_id_header,
            "user_agent": user_agent,
            "borrowed": borrowed,
        })
    }

    fn fake(describe: Value, answer: Result<Value, &str>) -> Arc<dyn ProbeModule> {
        let origin = https_origin(describe["url"].as_str().unwrap_or_default());
        Arc::new(Fake {
            describe: describe.to_string(),
            answer: answer.map(|value| value.to_string()).map_err(str::to_owned),
            origins: origin.into_iter().collect(),
        })
    }

    /// The five connector probes, each answering `{}` (no windows).
    struct Probes;

    impl ProbeModules for Probes {
        fn names(&self) -> Vec<String> {
            ["claude", "codex", "glm", "kimi", "opencode-go"]
                .map(str::to_owned)
                .to_vec()
        }

        fn module(&self, name: &str) -> Result<Option<Arc<dyn ProbeModule>>, String> {
            Ok(self
                .names()
                .contains(&name.to_owned())
                .then(|| fake(description(name), Ok(json!({})))))
        }
    }

    /// The connectors' two OAuth methods as a credential composition: only what a probe asks
    /// of a method (its usage probe and whether it borrows a login), with no component.
    struct Connectors;

    struct Described(String);

    impl p1_auth::CredentialModule for Described {
        fn describe(&self) -> Result<String, String> {
            Ok(self.0.clone())
        }
        fn read_login(&self, _: &[u8], _: Option<&str>) -> Result<String, String> {
            Err("not read in these tests".into())
        }
        fn refresh_request(&self, _: &str, _: Option<&[u8]>) -> Result<String, String> {
            Err("not refreshed in these tests".into())
        }
        fn refresh_response(&self, _: &[u8], _: u64) -> Result<String, String> {
            Err("not refreshed in these tests".into())
        }
        fn merge_login(
            &self,
            _: &[u8],
            _: &[u8],
            _: u64,
            _: u64,
            _: bool,
        ) -> Result<Vec<u8>, String> {
            Err("not refreshed in these tests".into())
        }
        fn token_origins(&self) -> &[String] {
            &[]
        }
    }

    impl p1_auth::CredentialModules for Connectors {
        fn names(&self) -> Vec<String> {
            vec!["claude-code-oauth".into(), "codex-oauth".into()]
        }

        fn module(&self, name: &str) -> Result<Option<Arc<dyn p1_auth::CredentialModule>>, String> {
            let probe = match name {
                "claude-code-oauth" => "claude",
                "codex-oauth" => "codex",
                _ => return Ok(None),
            };
            let text = json!({
                "method": {"owner": "a CLI", "store": "oauth", "usage_probe": probe},
                "login": {"title": "a login", "guidance": "log in", "home_dir": ".x", "file": "f"},
            })
            .to_string();
            Ok(Some(Arc::new(Described(text))))
        }
    }

    fn connectors() -> Locations {
        Locations::none().with_modules(Arc::new(Connectors))
    }

    fn claude_code_oauth() -> CredentialKind {
        CredentialKind::module("claude-code-oauth")
    }

    fn codex_oauth() -> CredentialKind {
        CredentialKind::module("codex-oauth")
    }

    fn fixture_route(id: &str) -> UsageRoute {
        fixture_route_of(id, CredentialKind::API_KEY)
    }

    fn fixture_route_of(id: &str, kind: CredentialKind) -> UsageRoute {
        UsageRoute {
            route_id: id.into(),
            label: id.into(),
            credential: "test source".into(),
            store_id: None,
            probe: None,
            compiled_origins: None,
            spec: p1_auth::CredentialSpec {
                kind,
                env: None,
                borrow: vec![],
                store_only: false,
                login_dir: None,
            },
        }
    }

    /// The url of `route`'s probe endpoint, or `None` when it has none.
    fn url_of(route: &UsageRoute) -> Option<String> {
        endpoint(route, &connectors(), &Probes)
            .ok()
            .flatten()
            .map(|endpoint| endpoint.url)
    }

    /// `module`'s answer for any body, as the ledger keeps it.
    fn decoded(answer: Result<Value, &str>) -> RouteUsage {
        let module = fake(description("kimi"), answer);
        decode_usage(&fixture_route("fixture"), module.as_ref(), b"{}")
    }

    /// ADR-0139: an account names its probe; the probe still takes only the credential
    /// method it is built for, and an unknown name probes nothing.
    #[test]
    fn a_named_probe_needs_its_own_credential_method() {
        let named = |probe: &str, kind: CredentialKind| {
            let mut route = fixture_route_of("any-account", kind);
            route.probe = Some(probe.into());
            url_of(&route)
        };
        assert_eq!(
            named("opencode-go", CredentialKind::API_KEY).as_deref(),
            Some("https://opencode.ai/zen/go/v1/usage")
        );
        assert_eq!(
            named("claude", claude_code_oauth()).as_deref(),
            Some("https://api.anthropic.com/api/oauth/usage")
        );
        assert_eq!(named("claude", CredentialKind::API_KEY), None);
        assert_eq!(named("opencode-go", CredentialKind::NONE), None);
        assert_eq!(named("unknown", CredentialKind::API_KEY), None);
        let mut route = fixture_route("any-account");
        assert_eq!(route.store_id(), "any-account");
        route.store_id = Some("old-route-id".into());
        assert_eq!(route.store_id(), "old-route-id");
    }

    /// ADR-0139: an API-key account gets its probe only from the `usage` name it carries; the
    /// id decides nothing, so an unnamed key account is never guessed at. The OAuth methods
    /// keep deriving their probe from the method.
    #[test]
    fn an_api_key_probe_comes_from_its_name_not_the_id() {
        let key_url = |id: &str, probe: Option<&str>| {
            let mut route = fixture_route_of(id, CredentialKind::API_KEY);
            route.probe = probe.map(str::to_string);
            url_of(&route)
        };
        // No `usage`, whatever the id: no endpoint is invented (this covers the free Zen and
        // ClinePass accounts, which name none).
        assert_eq!(key_url("some-key-route", None), None);
        assert_eq!(
            key_url("any-account", Some("kimi")).as_deref(),
            Some("https://api.kimi.ai/coding/v1/usages")
        );
        assert_eq!(
            key_url("any-account", Some("glm")).as_deref(),
            Some("https://api.z.ai/api/monitor/usage/quota/limit")
        );
        let oauth = |kind| url_of(&fixture_route_of("any-account", kind));
        assert_eq!(
            oauth(claude_code_oauth()).as_deref(),
            Some("https://api.anthropic.com/api/oauth/usage")
        );
        assert_eq!(
            oauth(codex_oauth()).as_deref(),
            Some("https://chatgpt.com/backend-api/wham/usage")
        );
    }

    /// ADR-0214: a probe's endpoint must be https at an origin its manifest declares, and it
    /// may not set the credential or the client's own headers; a name no probe is installed
    /// under says so.
    #[tokio::test]
    async fn a_probe_reaches_only_its_declared_origin_with_its_own_headers() {
        let mut moved = description("glm");
        moved["url"] = json!("https://elsewhere.example/usage");
        let declared = Arc::new(Fake {
            describe: moved.to_string(),
            answer: Ok("{}".into()),
            origins: vec!["https://api.z.ai".into()],
        });
        let reason = Endpoint::of(declared).err().expect("an undeclared origin");
        assert!(reason.contains("https://elsewhere.example"), "{reason}");
        for (field, value) in [
            ("url", json!("http://api.z.ai/usage")),
            ("url", json!("https://user@api.z.ai/usage")),
            ("headers", json!([["Authorization", "Bearer x"]])),
            ("headers", json!([["user-agent", "not-p1"]])),
            ("headers", json!([["x-ok", "line\nbreak"]])),
            ("account_id_header", json!("Cookie")),
            ("pigeon", json!(true)),
        ] {
            let mut text = description("glm");
            text[field] = value.clone();
            assert!(
                Endpoint::of(fake(text, Ok(json!({})))).is_err(),
                "{field} = {value}"
            );
        }
        let mut route = fixture_route("an-account");
        route.probe = Some("uninstalled".into());
        let result = HttpProbe::new(Arc::new(Probes))
            .probe(&route, &Locations::none(), Arc::new(NoNetwork))
            .await;
        assert!(
            matches!(&result.probe, Probe::Unsupported { reason } if reason == "usage probe `uninstalled` is not installed"),
            "{:?}",
            result.probe
        );
    }

    #[test]
    fn borrowed_probe_origins_follow_the_probe_and_its_method() {
        for kind in [claude_code_oauth(), codex_oauth()] {
            let route = fixture_route_of("borrowed-new-id", kind);
            let endpoint = endpoint(&route, &connectors(), &Probes).unwrap().unwrap();
            assert!(!check_probe_origin(&route, &endpoint, &connectors()).unwrap());
        }
        let mut kimi = fixture_route("kimi-coding-subscription");
        kimi.probe = Some("kimi".into());
        kimi.spec.borrow.push(p1_auth::BorrowSource {
            store: "pi".into(),
            key: "kimi".into(),
        });
        let endpoint = endpoint(&kimi, &connectors(), &Probes).unwrap().unwrap();
        assert!(
            check_probe_origin(&kimi, &endpoint, &Locations::none())
                .unwrap_err()
                .contains("not approved")
        );
    }

    #[tokio::test]
    async fn unapproved_store_probes_read_no_key_variable() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for (id, kind, probe) in [
            ("private-oauth", claude_code_oauth(), None),
            ("private-codex", codex_oauth(), None),
            ("private-key-route", CredentialKind::API_KEY, Some("kimi")),
            ("another-key-route", CredentialKind::API_KEY, Some("glm")),
        ] {
            let reads = Arc::new(AtomicUsize::new(0));
            let count = reads.clone();
            let locations = connectors().with_env_lookup(move |name| {
                if name == "TEST_KEY" {
                    count.fetch_add(1, Ordering::SeqCst);
                }
                None
            });
            let mut route = fixture_route_of(id, kind);
            route.probe = probe.map(str::to_string);
            route.spec.store_only = true;
            route.spec.env = Some("TEST_KEY".into());
            let result = HttpProbe::new(Arc::new(Probes))
                .probe(&route, &locations, Arc::new(NoNetwork))
                .await;
            assert_eq!(reads.load(Ordering::SeqCst), 0);
            let Probe::Unsupported { reason } = result.probe else {
                panic!("expected origin refusal");
            };
            assert!(reason.contains(&format!("p1 login {id} --trust-endpoint")));
        }
    }

    /// Issue #134: a route that sends no credential has no usage endpoint p1 could
    /// probe — it has no credential to present — so it is never mapped to one.
    #[test]
    fn a_route_that_sends_no_credential_maps_to_no_endpoint() {
        let route = fixture_route_of("proxy-route", CredentialKind::NONE);
        assert!(url_of(&route).is_none());
    }

    /// A transport that fails the test if a probe ever reaches the network.
    struct NoNetwork;

    impl Transport for NoNetwork {
        fn post<'a>(
            &'a self,
            _request: p1_provider_http::HttpRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            p1_provider_http::HttpResponse,
                            p1_provider_http::TransportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async {
                panic!("a route that sends no credential must not reach the network")
            })
        }
    }

    /// Issue #134: such a route is reported `Unsupported`, with a reason that says p1
    /// has no credential to present — and not a single request is made.
    #[tokio::test]
    async fn a_route_that_sends_no_credential_is_unsupported() {
        let route = fixture_route_of("proxy-route", CredentialKind::NONE);
        let result = HttpProbe::new(Arc::new(Probes))
            .probe(&route, &Locations::none(), Arc::new(NoNetwork))
            .await;
        match result.probe {
            Probe::Unsupported { reason } => {
                for part in ["none", "no credential"] {
                    assert!(reason.contains(part), "{reason}: {part}");
                }
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        assert!(result.windows.is_empty());
    }

    /// Without an installed probe, no route has a usage endpoint, and none is guessed.
    #[tokio::test]
    async fn without_probes_every_route_is_unsupported() {
        let mut route = fixture_route("anthropic");
        route.probe = Some("claude".into());
        let result = HttpProbe::new(Arc::new(NoProbes))
            .probe(&route, &Locations::none(), Arc::new(NoNetwork))
            .await;
        assert!(matches!(result.probe, Probe::Unsupported { .. }));
    }

    #[test]
    fn an_answer_keeps_its_windows_credits_and_extra_usage() {
        let usage = decoded(Ok(json!({
            "plan": "prolite",
            "windows": [
                {"kind": "session", "used_percent": 67.0, "resets_at": "2026-09-22T23:00:00Z", "limit_reached": false},
                {"kind": "weekly-scoped", "scope": "Fable", "used_percent": 7.0, "limit_reached": true},
                {"kind": "other", "label": "month code", "detail": "87/100 left", "limit_reached": false},
                {"kind": "other", "label": "a\u{7}b", "limit_reached": false},
            ],
            "credits": {"balance": 0.0, "reset_credits": 1},
            "extra_usage": {"enabled": true, "used": 12.4, "limit": 5000.0, "currency": "eur"},
        })));
        assert!(matches!(usage.probe, Probe::Supported));
        assert_eq!(usage.plan.as_deref(), Some("prolite"));
        assert!(matches!(usage.windows[0].kind, WindowKind::Session));
        assert_eq!(
            usage.windows[0].resets_at.as_deref(),
            Some("2026-09-22T23:00:00Z")
        );
        assert!(matches!(usage.windows[1].kind, WindowKind::WeeklyScoped));
        assert_eq!(usage.windows[1].scope.as_deref(), Some("Fable"));
        assert!(usage.windows[1].limit_reached);
        assert!(matches!(&usage.windows[2].kind, WindowKind::Other(name) if name == "month code"));
        assert_eq!(usage.windows[2].detail.as_deref(), Some("87/100 left"));
        // A label that is not one is shown as `other`.
        assert!(matches!(&usage.windows[3].kind, WindowKind::Other(name) if name == "other"));
        assert_eq!(usage.credits.as_ref().unwrap().reset_credits, Some(1));
        let extra = usage.extra_usage.as_ref().unwrap();
        assert_eq!(extra.used, Some(12.4));
        assert_eq!(extra.currency.as_deref(), Some("EUR"));
    }

    #[test]
    fn a_refused_credential_is_a_fixed_credential_failure() {
        let usage = decoded(Ok(json!({"credential_refused": true, "windows": [
            {"kind": "session", "used_percent": 5.0, "limit_reached": false}]})));
        assert!(matches!(usage.probe, Probe::Failed {
            kind: FailKind::Credential,
            ref detail,
        } if detail == "usage credential refused"));
        assert!(usage.windows.is_empty());
    }

    #[test]
    fn an_unreadable_body_or_answer_is_a_parse_failure() {
        for answer in [
            Err("the body is not JSON"),
            Ok(json!("not an answer")),
            Ok(json!({"windows": [{"kind": "fortnight", "limit_reached": false}]})),
            Ok(json!({"windows": [], "pigeon": 1})),
        ] {
            let usage = decoded(answer.clone());
            assert!(
                matches!(
                    usage.probe,
                    Probe::Failed {
                        kind: FailKind::Parse,
                        ..
                    }
                ),
                "{answer:?}"
            );
            assert!(usage.windows.is_empty());
        }
    }

    #[tokio::test]
    async fn compiled_probe_approval_does_not_extend_to_another_origin() {
        let mut route = fixture_route("a-shipped-account");
        route.probe = Some("glm".into());
        route.compiled_origins = Some(vec!["https://chat.example".into()]);
        let result = HttpProbe::new(Arc::new(Probes))
            .probe(&route, &Locations::none(), Arc::new(NoNetwork))
            .await;
        assert!(matches!(result.probe, Probe::Unsupported { .. }));
    }

    /// A refused key is a credential failure on a key route and an HTTP status on an OAuth
    /// route, and no failure carries a body.
    #[test]
    fn api_key_statuses_map_to_credential_or_http() {
        let body = br#"{"error":{"code":"unauthorized","detail":"SENTINEL_PRIVATE_VALUE"}}"#;
        let endpoint_of =
            |name: &str| Endpoint::of(fake(description(name), Ok(json!({})))).unwrap();
        for name in ["opencode-go", "kimi", "glm"] {
            let endpoint = endpoint_of(name);
            for status in [401, 402] {
                match endpoint.failure(status, body) {
                    Probe::Failed {
                        kind: FailKind::Credential,
                        detail,
                    } => assert_eq!(detail, format!("HTTP {status}")),
                    other => panic!("{status} did not map to a credential failure: {other:?}"),
                }
            }
            assert!(matches!(
                endpoint.failure(500, body),
                Probe::Failed {
                    kind: FailKind::Http(500),
                    ..
                }
            ));
        }
        assert!(matches!(
            endpoint_of("claude").failure(401, body),
            Probe::Failed {
                kind: FailKind::Http(401),
                ..
            }
        ));
    }

    struct FakeProbe {
        barrier: tokio::sync::Barrier,
        bearer: String,
    }
    impl UsageProbe for FakeProbe {
        fn probe<'a>(
            &'a self,
            route: &'a UsageRoute,
            _: &'a Locations,
            _: Arc<dyn Transport>,
        ) -> ProbeFuture<'a> {
            Box::pin(async move {
                self.barrier.wait().await;
                let mut result = RouteUsage::empty(route);
                if route.route_id == "broken" {
                    result.probe = Probe::Failed {
                        kind: FailKind::Network,
                        detail: "request failed".into(),
                    };
                } else {
                    result.windows.push(Window {
                        kind: WindowKind::Weekly,
                        scope: None,
                        used_percent: Some(self.bearer.len() as f64),
                        resets_at: None,
                        limit_reached: false,
                        detail: None,
                    });
                }
                result
            })
        }
    }

    #[tokio::test]
    async fn probe_concurrency_isolation_and_redaction() {
        let sentinel = "SENTINEL_PRIVATE_VALUE";
        let probe = FakeProbe {
            barrier: tokio::sync::Barrier::new(3),
            bearer: sentinel.into(),
        };
        let routes = ["good", "broken", "also-good"].map(|id| UsageRoute {
            route_id: id.into(),
            ..fixture_route("fixture")
        });
        let snapshot = snapshot_with(
            &routes,
            &Locations::none(),
            Arc::new(p1_provider_http::ReqwestTransport::new()),
            &probe,
        )
        .await;
        assert!(matches!(snapshot.routes[1].probe, Probe::Failed { .. }));
        assert!(matches!(snapshot.routes[0].probe, Probe::Supported));
        assert!(matches!(snapshot.routes[2].probe, Probe::Supported));
        assert!(!format!("{snapshot:?}").contains(sentinel));
        assert!(!serde_json::to_string(&snapshot).unwrap().contains(sentinel));
    }

    /// Two runtime-built stand-ins for a credential: one shaped like a key, and one plain
    /// enough to pass every shape rule, which only the exact check against what the probe
    /// sent can catch.
    fn seeded() -> [String; 2] {
        [
            format!("Kx{}", "9aQ".repeat(10)),
            "plainlowercasetokenvalue".to_string(),
        ]
    }

    /// Everything a snapshot keeps and shows of one route.
    fn visible(usage: &RouteUsage) -> String {
        let snapshot = Snapshot {
            taken_at: "2026-09-24T00:06:00Z".into(),
            routes: vec![usage.clone()],
        };
        let rows: Vec<String> = crate::render::render(&snapshot, 48)
            .iter()
            .map(|line| line.text())
            .collect();
        format!(
            "{}\n{snapshot:?}\n{}",
            serde_json::to_string(&snapshot).unwrap(),
            rows.join("\n")
        )
    }

    /// Review #484 findings 14/15: a probe answer that puts the credential into every string
    /// the snapshot would keep leaves no copy in the snapshot, its JSON or its rows.
    #[test]
    fn retained_strings_never_echo_the_credential() {
        for secret in seeded() {
            let out = decoded(Ok(json!({
                "plan": secret,
                "windows": [
                    {"kind": "other", "label": secret, "scope": secret, "used_percent": 5.0,
                     "resets_at": secret, "detail": format!("{secret}/{secret} left"),
                     "limit_reached": false},
                ],
                "extra_usage": {"enabled": true, "currency": secret},
            })));
            let out = Sent::of(&secret).forget_echoes(out);
            let shown = visible(&out);
            assert!(!shown.contains(&secret), "{shown}");
        }
    }

    /// An error code that IS the credential, or is shaped like one, is never the detail.
    #[test]
    fn an_echoed_error_code_is_never_the_failure_detail() {
        for secret in seeded() {
            for status in [400, 401, 500] {
                let body = format!(r#"{{"error":{{"code":"{secret}"}}}}"#);
                for name in ["claude", "codex", "kimi"] {
                    let endpoint = Endpoint::of(fake(description(name), Ok(json!({})))).unwrap();
                    let mut out = RouteUsage::empty(&fixture_route("fixture"));
                    out.probe = endpoint.failure(status, body.as_bytes());
                    let out = Sent::of(&secret).forget_echoes(out);
                    let shown = visible(&out);
                    assert!(!shown.contains(&secret), "{shown}");
                    assert!(
                        matches!(&out.probe, Probe::Failed { detail, .. } if detail.starts_with(&format!("HTTP {status}"))),
                        "{:?}",
                        out.probe
                    );
                }
            }
        }
        // A plain code is still shown.
        assert!(matches!(
            http_failure(429, br#"{"error":{"type":"rate_limit_error"}}"#),
            Probe::Failed { detail, .. } if detail == "HTTP 429 · rate_limit_error"
        ));
    }

    /// A credential error's text reaches the snapshot masked and bounded.
    #[test]
    fn a_credential_error_detail_is_masked() {
        let [shaped, _] = seeded();
        let detail = masked_detail(&format!(
            "the p1 store /home/owner/{shaped}/p1/auth.json could not be read; token {shaped}"
        ));
        assert!(!detail.contains(&shaped), "{detail}");
        assert!(detail.contains("could not be read"), "{detail}");
        assert!(masked_detail(&"word ".repeat(200)).chars().count() <= DETAIL_LIMIT + 1);
    }

    #[test]
    fn labels_timestamps_and_currencies_keep_only_their_own_shape() {
        assert_eq!(label("Opus (1M)").as_deref(), Some("Opus (1M)"));
        assert_eq!(label("a\u{7}b"), None);
        assert_eq!(label(&"x".repeat(LABEL_LIMIT + 1)), None);
        assert_eq!(currency("eur").as_deref(), Some("EUR"));
        assert_eq!(currency("EURO"), None);
        assert_eq!(
            reset_text("2026-09-24T00:51:16.925Z").as_deref(),
            Some("2026-09-24T00:51:16.925Z")
        );
        assert_eq!(reset_text("tomorrow"), None);
        assert_eq!(
            detail("12345678901234567890/12345678901234567890 left").as_deref(),
            Some("12345678901234567890/12345678901234567890 left")
        );
    }
}
