//! ADR-0074 with ADR-0054, end to end through the host: a workflow role on the first
//! Claude subscription (`claude/…`) whose account is out of quota moves its step to the
//! second subscription (`claude2/…`). No engine change is involved — an exhausted
//! account is a route failure (ADR-0054) — so this drives the SHIPPED environments and
//! route files through the real Messages adapter on one scripted transport: the first
//! route answers `429 rate_limit_error` until its retries are spent, the second answers
//! the step. Each route reads its own fake credential from a scratch home: the first
//! from p1's store (it is store-only), the second from the Claude Code login in its
//! `login_dir` (`~/.claude-2`). No test here reads a real login or opens a socket.
#![cfg(feature = "workflows")]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{Harness, run_args, shipped_environments};
use p1_provider_http::HttpRequest;
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use serde_json::json;

const FIRST_BEARER: &str = "FAKE-FIRST-ACCOUNT-TOKEN";
const SECOND_BEARER: &str = "FAKE-SECOND-ACCOUNT-TOKEN";

/// Every role resolves at preflight; only the worker has a chain, from the first
/// Claude account to the second.
const ROLES: &str = r#"
[workflows.roles.worker]
model = "claude/claude-sonnet-5"
tools = ["read"]
fallback = ["claude2/claude-sonnet-5"]
[workflows.roles.reviewer]
model = "claude/claude-sonnet-5"
tools = ["read"]
[workflows.roles.verifier]
model = "claude/claude-sonnet-5"
tools = ["read"]
[workflows.roles.judge]
model = "claude/claude-sonnet-5"
tools = ["read"]
"#;

/// How many times the first account is asked: the request and the Messages driver's
/// default three transient retries (`RetryPolicy::default`).
const FIRST_ACCOUNT_ATTEMPTS: usize = 4;

/// The first account's quota is used up: Anthropic's `rate_limit_error`, with a
/// `retry-after` of zero so the driver's retries need no wait.
fn quota_exhausted() -> ScriptedResponse {
    ScriptedResponse {
        status: 429,
        headers: vec![("retry-after".to_string(), "0".to_string())],
        chunks: vec![
            br#"{"type":"error","error":{"type":"rate_limit_error","message":"quota"}}"#.to_vec(),
        ],
        end: BodyEnd::Eof,
    }
}

/// A Messages turn that calls `finish` with `done`.
fn finish_turn() -> ScriptedResponse {
    let input = json!({
        "status": "done",
        "summary": "ran on the second account",
        "verification": ["none"],
    })
    .to_string();
    let delta = json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": { "type": "input_json_delta", "partial_json": input },
    });
    ScriptedResponse::ok_sse(&format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_finish\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":1}}}}}}\n\n\
         event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"call_finish\",\"name\":\"finish\"}}}}\n\n\
         event: content_block_delta\n\
         data: {delta}\n\n\
         event: content_block_stop\n\
         data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":5}}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n"
    ))
}

/// The worker's closing text turn after `finish`.
fn text_turn() -> ScriptedResponse {
    ScriptedResponse::ok_sse(p1_provider_conformance::fixtures::anthropic::text_turn)
}

fn write(path: &Path, text: &str, dir_mode: u32) {
    let parent = path.parent().unwrap();
    std::fs::create_dir_all(parent).unwrap();
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(dir_mode)).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn bearer_of(request: &HttpRequest) -> String {
    request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn an_exhausted_first_claude_account_moves_the_step_to_the_second() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let config = root.path().join("config");
        write(&config.join("p1/settings.toml"), ROLES, 0o700);
        // The first route is store-only: its token lives in p1's (private) store.
        write(
            &config.join("p1/auth.json"),
            &json!({
                "anthropic-subscription": {
                    "type": "oauth",
                    "access": FIRST_BEARER,
                    "refresh": null,
                    "expires": null,
                    "account_id": null,
                }
            })
            .to_string(),
            0o700,
        );
        // The second route borrows the Claude Code login in its `login_dir`.
        write(
            &home.join(".claude-2/.credentials.json"),
            &json!({
                "claudeAiOauth": {
                    "accessToken": SECOND_BEARER,
                    "refreshToken": "FAKE-SECOND-REFRESH",
                    "expiresAt": 4_102_444_800_000u64,
                }
            })
            .to_string(),
            0o700,
        );
        let script = workspace.path().join("fallback.rhai");
        std::fs::write(&script, r#"agent("the task", #{ label: "one" })"#).unwrap();
        let out = root.path().join("runs");

        let mut responses: Vec<ScriptedResponse> = (0..FIRST_ACCOUNT_ATTEMPTS)
            .map(|_| quota_exhausted())
            .collect();
        responses.push(finish_turn());
        responses.push(text_turn());
        let transport = ScriptedTransport::new(responses);

        let mut harness = Harness::new(vec![shipped_environments()], &[]);
        harness.deps.transport = Arc::new(transport.clone());
        harness.deps.home = Some(home.clone());
        harness.deps.shell_env = Some(vec![
            ("HOME".into(), home.clone().into_os_string()),
            ("XDG_CONFIG_HOME".into(), config.clone().into_os_string()),
            (
                "XDG_STATE_HOME".into(),
                root.path().join("state").into_os_string(),
            ),
        ]);
        let code = run_args(
            &mut harness,
            &[
                "workflow",
                "run",
                script.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
                "--workspace",
                workspace.path().to_str().unwrap(),
            ],
        )
        .await;
        let stderr = harness.stderr.text();
        assert_eq!(code, 0, "completed: {stderr}");

        // The first account was asked until its retries were spent, then the second
        // account answered the step — each with its own credential.
        let bearers: Vec<String> = transport.requests().iter().map(bearer_of).collect();
        let mut expected = vec![format!("Bearer {FIRST_BEARER}"); FIRST_ACCOUNT_ATTEMPTS];
        expected.extend(vec![format!("Bearer {SECOND_BEARER}"); 2]);
        assert_eq!(bearers, expected, "{stderr}");

        let result: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.join("wf1/result.json")).unwrap())
                .unwrap();
        assert_eq!(result["counts"]["done"], 1, "{result}");
        assert_eq!(result["counts"]["fell_back"], 1, "{result}");
        assert_eq!(
            result["steps"][0]["models"],
            json!([
                {"model": "claude/claude-sonnet-5", "moved_on": "route_failed"},
                {"model": "claude2/claude-sonnet-5", "moved_on": null},
            ]),
            "{result}"
        );
        assert!(
            stderr.contains("claude/claude-sonnet-5 route failed → claude2/claude-sonnet-5"),
            "the step line names the hop: {stderr}"
        );
    })
    .await
    .expect("the fallback workflow run hung");
}
