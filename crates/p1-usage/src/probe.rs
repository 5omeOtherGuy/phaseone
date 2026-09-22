use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::timestamp;
use futures_util::future::join_all;
use p1_auth::{CredentialKind, Locations};
use p1_provider_http::Transport;
use serde_json::Value;
use time::OffsetDateTime;

use crate::{
    Credits, ExtraUsage, FailKind, Probe, RouteUsage, Snapshot, UsageRoute, Window, WindowKind,
};

type ProbeFuture<'a> = Pin<Box<dyn Future<Output = RouteUsage> + Send + 'a>>;

/// Injectable probe boundary: each route finishes independently of the others.
pub trait UsageProbe: Send + Sync {
    fn probe<'a>(
        &'a self,
        route: &'a UsageRoute,
        locations: &'a Locations,
        transport: Arc<dyn Transport>,
    ) -> ProbeFuture<'a>;
}

pub struct HttpProbe;

impl UsageProbe for HttpProbe {
    fn probe<'a>(
        &'a self,
        route: &'a UsageRoute,
        locations: &'a Locations,
        transport: Arc<dyn Transport>,
    ) -> ProbeFuture<'a> {
        Box::pin(async move {
            let mut result = RouteUsage::empty(route);
            let (url, beta) = match route.spec.kind {
                CredentialKind::ClaudeCodeOauth => {
                    ("https://api.anthropic.com/api/oauth/usage", true)
                }
                CredentialKind::CodexOauth => ("https://chatgpt.com/backend-api/wham/usage", false),
                CredentialKind::ApiKey => {
                    result.probe = Probe::Unsupported {
                        reason: "no usage endpoint known for this route".into(),
                    };
                    return result;
                }
            };
            let source = p1_auth::resolve(&route.route_id, &route.spec, transport, locations);
            let credential = match source.access().await {
                Ok(value) => value,
                Err(error) => {
                    result.probe = Probe::Failed {
                        kind: FailKind::Credential,
                        detail: error.to_string(),
                    };
                    return result;
                }
            };
            if !beta && credential.account_id.is_none() {
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
            // Secret material is used only in request construction; no response or error carries it onward.
            let mut request = client.get(url).bearer_auth(&credential.bearer);
            if beta {
                request = request.header("anthropic-beta", "oauth-2025-04-20");
            } else if let Some(account_id) = &credential.account_id {
                request = request.header("ChatGPT-Account-Id", account_id);
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
            if !status.is_success() {
                result.probe = http_failure(status.as_u16(), &bytes);
                return result;
            }
            decode_response(&bytes, beta, result)
        })
    }
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
        .filter(|s| {
            s.len() <= 40
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        });
    Probe::Failed {
        kind: FailKind::Http(status),
        detail: match code {
            Some(code) => format!("HTTP {status} · {code}"),
            None => format!("HTTP {status}"),
        },
    }
}

fn decode_response(bytes: &[u8], claude: bool, mut out: RouteUsage) -> RouteUsage {
    if parse_response(bytes, claude, &mut out).is_err() {
        out.probe = Probe::Failed {
            kind: FailKind::Parse,
            detail: "invalid usage response".into(),
        };
        out.windows.clear();
        out.credits = None;
        out.extra_usage = None;
        out.plan = None;
    }
    out
}

fn parse_response(bytes: &[u8], claude: bool, out: &mut RouteUsage) -> Result<(), ()> {
    let data: Value = serde_json::from_slice(bytes).map_err(|_| ())?;
    if claude {
        let limits = data.get("limits").and_then(Value::as_array).ok_or(())?;
        for limit in limits {
            let kind = match limit.get("kind").and_then(Value::as_str).ok_or(())? {
                "session" => WindowKind::Session,
                "weekly_all" => WindowKind::Weekly,
                "weekly_scoped" => WindowKind::WeeklyScoped,
                other => WindowKind::Other(other.to_string()),
            };
            out.windows.push(Window {
                kind,
                scope: limit
                    .pointer("/scope/model/display_name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                used_percent: limit.get("percent").and_then(Value::as_f64),
                resets_at: limit
                    .get("resets_at")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                limit_reached: limit
                    .get("limit_reached")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
        if let Some(extra) = data.get("extra_usage").filter(|v| v.is_object()) {
            out.extra_usage = Some(ExtraUsage {
                enabled: extra.get("is_enabled").and_then(Value::as_bool),
                used: number(extra.get("used_credits")),
                limit: number(extra.get("monthly_limit")),
                currency: extra
                    .get("currency")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    } else {
        out.plan = data
            .get("plan_type")
            .and_then(Value::as_str)
            .map(str::to_string);
        let rate = data
            .get("rate_limit")
            .and_then(Value::as_object)
            .ok_or(())?;
        let reached = rate
            .get("limit_reached")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        for key in ["primary_window", "secondary_window"] {
            let Some(window) = rate.get(key).filter(|v| v.is_object()) else {
                continue;
            };
            let seconds = window.get("limit_window_seconds").and_then(Value::as_u64);
            let kind = match seconds {
                Some(18_000) => WindowKind::Session,
                Some(604_800) => WindowKind::Weekly,
                Some(n) => WindowKind::Other(format!("{}h", n / 3600)),
                None => WindowKind::Other("unknown".into()),
            };
            let resets_at = window
                .get("reset_at")
                .and_then(Value::as_i64)
                .and_then(|n| OffsetDateTime::from_unix_timestamp(n).ok())
                .map(timestamp::format);
            out.windows.push(Window {
                kind,
                scope: None,
                used_percent: window.get("used_percent").and_then(Value::as_f64),
                resets_at,
                limit_reached: reached,
            });
        }
        let balance = number(data.pointer("/credits/balance"));
        let reset_credits = data
            .pointer("/rate_limit_reset_credits/available_count")
            .and_then(Value::as_u64);
        if balance.is_some() || reset_credits.is_some() {
            out.credits = Some(Credits {
                balance,
                reset_credits,
            });
        }
    }
    Ok(())
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

pub async fn snapshot(
    routes: &[UsageRoute],
    locations: &Locations,
    transport: Arc<dyn Transport>,
) -> Snapshot {
    snapshot_with(routes, locations, transport, &HttpProbe).await
}

pub async fn snapshot_with<P: UsageProbe + ?Sized>(
    routes: &[UsageRoute],
    locations: &Locations,
    transport: Arc<dyn Transport>,
    probe: &P,
) -> Snapshot {
    let taken_at = timestamp::format(OffsetDateTime::now_utc());
    let routes = join_all(
        routes
            .iter()
            .map(|route| probe.probe(route, locations, transport.clone())),
    )
    .await;
    Snapshot { taken_at, routes }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn probe_fixtures() {
        let mut claude = RouteUsage::empty(&fixture_route());
        parse_response(br#"{"limits":[{"kind":"session","percent":67,"resets_at":"2026-09-22T23:00:00Z"},{"kind":"weekly_scoped","percent":7,"scope":{"model":{"display_name":"Fable"}}}],"extra_usage":{"is_enabled":true,"used_credits":12.4,"monthly_limit":5000,"currency":"EUR"}}"#, true, &mut claude).unwrap();
        assert!(matches!(claude.windows[1].kind, WindowKind::WeeklyScoped));
        assert_eq!(claude.windows[1].scope.as_deref(), Some("Fable"));
        assert_eq!(claude.extra_usage.as_ref().unwrap().used, Some(12.4));
        let mut codex = RouteUsage::empty(&fixture_route());
        parse_response(br#"{"plan_type":"prolite","rate_limit":{"primary_window":{"used_percent":69,"limit_window_seconds":604800,"reset_at":1790451360},"limit_reached":true},"credits":{"balance":0},"rate_limit_reset_credits":{"available_count":1}}"#, false, &mut codex).unwrap();
        assert_eq!(codex.windows.len(), 1);
        assert!(codex.windows[0].limit_reached);
        assert!(matches!(codex.windows[0].kind, WindowKind::Weekly));
        assert!(matches!(
            http_failure(401, br#"{"error":{"type":"unauthorized"}}"#),
            Probe::Failed {
                kind: FailKind::Http(401),
                ..
            }
        ));
        assert!(matches!(
            decode_response(b"not json", true, claude).probe,
            Probe::Failed {
                kind: FailKind::Parse,
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
            ..fixture_route()
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

    fn fixture_route() -> UsageRoute {
        UsageRoute {
            route_id: "fixture".into(),
            label: "fixture".into(),
            credential: "test source".into(),
            spec: p1_auth::CredentialSpec {
                kind: CredentialKind::ApiKey,
                env: None,
                borrow: vec![],
            },
        }
    }
}
