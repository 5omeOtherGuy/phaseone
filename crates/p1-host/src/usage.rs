//! The CLI adapter for the provider-neutral usage ledger.
use std::io::Write;

use p1_auth::Locations;
use p1_usage::{Tone, UsageRoute};

use crate::HostDeps;
use crate::cli::UsageOptions;

fn label(id: &str) -> String {
    match id {
        "anthropic-subscription" => "claude max".into(),
        "openai-codex-subscription" => "chatgpt pro lite".into(),
        "opencode-go-subscription" => "opencode go".into(),
        "opencode-go-2-subscription" => "opencode go-2".into(),
        "glm-subscription" => "glm".into(),
        _ => id
            .strip_suffix("-subscription")
            .unwrap_or(id)
            .replace('-', " "),
    }
}

pub async fn usage(deps: &HostDeps, options: &UsageOptions) -> i32 {
    let UsageOptions {
        json,
        watch,
        plain,
        grid,
        search,
    } = options;
    let (json, watch, plain, grid, search) = (*json, *watch, *plain, *grid, search.as_deref());
    let locations = Locations::from_process();
    let routes = match crate::routes::load_all_routes(&deps.environment_dirs) {
        Ok(routes) => routes,
        Err(error) => {
            let _ = writeln!(deps.stderr.lock().unwrap(), "{error}");
            return 2;
        }
    };
    let search = search.map(str::to_lowercase);
    let routes: Vec<UsageRoute> = routes
        .into_iter()
        .filter(|r| {
            search
                .as_ref()
                .is_none_or(|s| r.id.to_lowercase().contains(s) || label(&r.id).contains(s))
        })
        .map(|route| UsageRoute {
            label: label(&route.id),
            credential: p1_auth::describe(&route.id, &route.credential, &locations).line(),
            route_id: route.id,
            spec: route.credential,
        })
        .collect();
    let interrupt = deps.interrupt.recv();
    tokio::pin!(interrupt);
    loop {
        let probe = p1_usage::snapshot(&routes, &locations, deps.transport.clone());
        let snapshot = if watch.is_some() {
            tokio::select! {
                _ = &mut interrupt => return 0,
                snapshot = probe => snapshot,
            }
        } else {
            probe.await
        };
        let output = if json {
            serde_json::to_string_pretty(&snapshot).unwrap_or_default()
        } else {
            let ansi = deps.stdout_is_tty && !plain;
            p1_usage::render(&snapshot, grid)
                .iter()
                .map(|line| {
                    let mut text = String::new();
                    for span in &line.0 {
                        if ansi && !span.text.is_empty() {
                            let rgb = match span.tone {
                                Tone::Ink => "232;232;232",
                                Tone::Dim => "154;154;154",
                                Tone::Faint => "106;106;106",
                                Tone::Rule => "42;42;42",
                            };
                            text.push_str(&format!("\x1b[38;2;{rgb}m{}\x1b[0m", span.text));
                        } else {
                            text.push_str(&span.text);
                        }
                    }
                    text
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        if watch.is_some() {
            let _ = write!(deps.stdout.lock().unwrap(), "\x1b[2J\x1b[H");
        }
        let _ = writeln!(deps.stdout.lock().unwrap(), "{output}");
        let Some(seconds) = watch else { return 0 };
        tokio::select! {
            _ = &mut interrupt => return 0,
            _ = tokio::time::sleep(std::time::Duration::from_secs(seconds)) => {},
        }
    }
}
