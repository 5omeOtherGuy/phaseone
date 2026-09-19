//! Plain, line-oriented event rendering.
//!
//! Assistant text is streamed to stdout as it arrives and newline-terminated when
//! the response completes. Reasoning is shown only on a TTY, dimmed. Tool
//! start/finish, inbox delivery and turn failures are one line each. Usage goes
//! to stderr after EVERY response, and a totals line at exit. Unknown usage is
//! printed `?`/`unknown`, never `0`.

use std::io::Write;
use std::sync::{Arc, Mutex};

use p1_contracts::{AgentEvent, EventSink, ToolStatus, TurnEnd, Usage};

use crate::SharedWriter;

/// State that must be updated atomically with the write it describes.
struct Inner {
    /// True when the next stdout character begins a line (so it needs the prefix).
    line_start: bool,
    last_model: String,
    in_total: u64,
    in_unknown: bool,
    cached_total: u64,
    cached_unknown: bool,
    out_total: u64,
    out_unknown: bool,
    cost_total: u64,
    cost_unknown: bool,
}

/// Renders agent events to the injected writers. One renderer is shared by the
/// parent agent; a child gets its own with a `[<child id>] ` prefix.
pub struct Renderer {
    stdout: SharedWriter,
    stderr: SharedWriter,
    tty: bool,
    route: String,
    model: String,
    prefix: Arc<Mutex<String>>,
    inner: Mutex<Inner>,
}

impl Renderer {
    pub fn new(
        stdout: SharedWriter,
        stderr: SharedWriter,
        tty: bool,
        route: String,
        model: String,
        prefix: Arc<Mutex<String>>,
    ) -> Self {
        Self {
            stdout,
            stderr,
            tty,
            route,
            model,
            prefix,
            inner: Mutex::new(Inner {
                line_start: true,
                last_model: String::new(),
                in_total: 0,
                in_unknown: false,
                cached_total: 0,
                cached_unknown: false,
                out_total: 0,
                out_unknown: false,
                cost_total: 0,
                cost_unknown: false,
            }),
        }
    }

    /// Print the totals line to stderr. Called once, at exit.
    pub fn finish(&self) {
        let mut inner = self.inner.lock().unwrap();
        self.close_line(&mut inner);
        let model = if inner.last_model.is_empty() {
            self.model.clone()
        } else {
            inner.last_model.clone()
        };
        let input = if inner.in_unknown {
            "?".to_string()
        } else {
            inner.in_total.to_string()
        };
        let cached = if inner.cached_unknown {
            "?".to_string()
        } else {
            inner.cached_total.to_string()
        };
        let output = if inner.out_unknown {
            "?".to_string()
        } else {
            inner.out_total.to_string()
        };
        let cost = if inner.cost_unknown {
            "unknown".to_string()
        } else {
            cost_string(Some(inner.cost_total))
        };
        let line = format!(
            "total model {}/{model} · in {input} (cached {cached}) · out {output} · cost {cost}",
            self.route
        );
        self.write_line(&mut inner, true, &line);
    }

    fn prefix(&self) -> String {
        self.prefix.lock().unwrap().clone()
    }

    fn raw_out(&self, text: &str) {
        let mut writer = self.stdout.lock().unwrap();
        let _ = writer.write_all(text.as_bytes());
        let _ = writer.flush();
    }

    fn raw_err(&self, text: &str) {
        let mut writer = self.stderr.lock().unwrap();
        let _ = writer.write_all(text.as_bytes());
        let _ = writer.flush();
    }

    fn close_line(&self, inner: &mut Inner) {
        if !inner.line_start {
            self.raw_out("\n");
            inner.line_start = true;
        }
    }

    /// Write one complete line, prefixed. `to_err` selects the stream.
    fn write_line(&self, inner: &mut Inner, to_err: bool, text: &str) {
        let line = format!("{}{text}\n", self.prefix());
        if to_err {
            self.raw_err(&line);
        } else {
            self.raw_out(&line);
        }
        inner.line_start = true;
    }

    fn write_text(&self, inner: &mut Inner, text: &str) {
        if text.is_empty() {
            return;
        }
        let prefix = self.prefix();
        let mut out = String::with_capacity(text.len() + prefix.len());
        for ch in text.chars() {
            if inner.line_start {
                out.push_str(&prefix);
                inner.line_start = false;
            }
            out.push(ch);
            if ch == '\n' {
                inner.line_start = true;
            }
        }
        self.raw_out(&out);
    }

    fn write_dim(&self, inner: &mut Inner, text: &str) {
        if text.is_empty() {
            return;
        }
        let prefix = self.prefix();
        let mut out = String::new();
        if !inner.line_start {
            out.push_str("\x1b[2m");
        }
        for ch in text.chars() {
            if inner.line_start {
                out.push_str(&prefix);
                out.push_str("\x1b[2m");
                inner.line_start = false;
            }
            out.push(ch);
            if ch == '\n' {
                out.push_str("\x1b[0m");
                inner.line_start = true;
            }
        }
        out.push_str("\x1b[0m");
        self.raw_out(&out);
    }

    fn record_usage(&self, inner: &mut Inner, usage: Option<Usage>) {
        match usage {
            None => {
                inner.in_unknown = true;
                inner.cached_unknown = true;
                inner.out_unknown = true;
                inner.cost_unknown = true;
            }
            Some(usage) => {
                match input_total(&usage) {
                    Some(total) => inner.in_total += total,
                    None => inner.in_unknown = true,
                }
                match usage.cache_read {
                    Some(value) => inner.cached_total += value,
                    None => inner.cached_unknown = true,
                }
                match usage.output {
                    Some(value) => inner.out_total += value,
                    None => inner.out_unknown = true,
                }
                match usage.cost_micro_usd {
                    Some(value) => inner.cost_total += value,
                    None => inner.cost_unknown = true,
                }
            }
        }
    }
}

impl EventSink for Renderer {
    fn emit(&self, event: AgentEvent) {
        let mut inner = self.inner.lock().unwrap();
        match event {
            AgentEvent::TurnStarted | AgentEvent::RequestStarted { .. } => {}
            AgentEvent::TextDelta { text } => self.write_text(&mut inner, &text),
            AgentEvent::ReasoningDelta { text } => {
                if self.tty {
                    self.write_dim(&mut inner, &text);
                }
            }
            AgentEvent::ToolInputDelta { .. } => {}
            AgentEvent::ResponseCompleted { model, usage, .. } => {
                self.close_line(&mut inner);
                let line = usage_line(&self.route, &model, usage);
                self.write_line(&mut inner, true, &line);
                self.record_usage(&mut inner, usage);
                inner.last_model = model;
            }
            AgentEvent::InboxDelivered { count } => {
                self.close_line(&mut inner);
                self.write_line(
                    &mut inner,
                    false,
                    &format!("• {count} notification(s) delivered"),
                );
            }
            AgentEvent::ToolStarted { call } => {
                self.close_line(&mut inner);
                let summary = summarize_input(call.input.raw());
                let line = if summary.is_empty() {
                    format!("→ {}", call.name)
                } else {
                    format!("→ {} {summary}", call.name)
                };
                self.write_line(&mut inner, false, &line);
            }
            AgentEvent::ToolFinished { result } => {
                self.close_line(&mut inner);
                let mut line = format!(
                    "← {} {} ({} lines)",
                    result.name,
                    status_name(result.status),
                    result.content.lines().count()
                );
                if result.status != ToolStatus::Ok
                    && let Some(first) = result.content.lines().next()
                    && !first.is_empty()
                {
                    line.push(' ');
                    line.push_str(first);
                }
                self.write_line(&mut inner, false, &line);
            }
            AgentEvent::TurnFinished { end } => {
                let line = match end {
                    TurnEnd::Completed { .. } => None,
                    TurnEnd::Cancelled => Some("! turn cancelled".to_string()),
                    TurnEnd::ProviderFailed { error } => {
                        Some(format!("! provider failed: {error}"))
                    }
                    TurnEnd::CommitFailed { message } => {
                        Some(format!("! commit failed: {message}"))
                    }
                    TurnEnd::ContextFailed { message } => {
                        Some(format!("! context failed: {message}"))
                    }
                };
                if let Some(line) = line {
                    self.close_line(&mut inner);
                    self.write_line(&mut inner, true, &line);
                }
            }
        }
    }
}

/// The exact usage line, `?`/`unknown` for anything not reported.
pub fn usage_line(route: &str, model: &str, usage: Option<Usage>) -> String {
    let (input, cached, output, cost) = match usage {
        None => (
            "?".to_string(),
            "?".to_string(),
            "?".to_string(),
            "unknown".to_string(),
        ),
        Some(usage) => (
            input_total(&usage).map_or("?".to_string(), |total| total.to_string()),
            option_count(usage.cache_read),
            option_count(usage.output),
            cost_string(usage.cost_micro_usd),
        ),
    };
    format!("model {route}/{model} · in {input} (cached {cached}) · out {output} · cost {cost}")
}

fn option_count(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn cost_string(micro_usd: Option<u64>) -> String {
    match micro_usd {
        None => "unknown".to_string(),
        Some(micro) => format!("${}.{:04}", micro / 1_000_000, (micro % 1_000_000) / 100),
    }
}

/// The one-line input summary shown for a tool start and in an authorization ask:
/// newlines become `␤`, at most 100 characters.
pub fn summarize_input(raw: &str) -> String {
    raw.replace('\n', "␤").chars().take(100).collect()
}

/// The short, stable name of a tool status.
pub fn status_name(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Ok => "ok",
        ToolStatus::Error => "error",
        ToolStatus::Unavailable => "unavailable",
        ToolStatus::Denied => "denied",
        ToolStatus::Cancelled => "cancelled",
        ToolStatus::Unknown => "unknown",
    }
}

/// Total input tokens of one response. Known as soon as the uncached part is known:
/// the cache parts are ADDED when the route reports them, and a route that has no such
/// concept (the Codex route never reports cache writes) does not make the total unknown.
fn input_total(usage: &Usage) -> Option<u64> {
    usage
        .input_uncached
        .map(|uncached| uncached + usage.cache_read.unwrap_or(0) + usage.cache_write.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_and_bounds_the_input() {
        assert_eq!(summarize_input("a\nb"), "a␤b");
        assert_eq!(summarize_input(&"x".repeat(200)).chars().count(), 100);
    }

    #[test]
    fn formats_known_and_unknown_usage() {
        let known = Usage {
            input_uncached: Some(10),
            cache_read: Some(5),
            cache_write: Some(2),
            output: Some(7),
            reasoning_output: None,
            cost_micro_usd: Some(12_300),
        };
        assert_eq!(
            usage_line("r", "m", Some(known)),
            "model r/m · in 17 (cached 5) · out 7 · cost $0.0123"
        );
        assert_eq!(
            usage_line("r", "m", None),
            "model r/m · in ? (cached ?) · out ? · cost unknown"
        );
        let partial = Usage {
            input_uncached: Some(10),
            cache_read: None,
            ..Usage::default()
        };
        assert_eq!(
            usage_line("r", "m", Some(partial)),
            "model r/m · in 10 (cached ?) · out ? · cost unknown"
        );
        // The Codex route: cached reads reported, cache writes not a concept.
        let codex = Usage {
            input_uncached: Some(84),
            cache_read: Some(16),
            output: Some(18),
            ..Usage::default()
        };
        assert_eq!(
            usage_line("r", "m", Some(codex)),
            "model r/m · in 100 (cached 16) · out 18 · cost unknown"
        );
    }
}
