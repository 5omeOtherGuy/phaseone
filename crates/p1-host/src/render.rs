//! Plain, line-oriented event rendering.
//!
//! Assistant text is streamed to stdout as it arrives and newline-terminated when
//! the response completes. Reasoning is shown only on a TTY, dimmed. Tool
//! start/finish, inbox delivery and turn failures are one line each. Usage goes
//! to stderr after EVERY response, and a totals line at exit. Unknown usage is
//! printed `?`/`unknown`, never `0`.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use p1_contracts::{AgentEvent, EventSink, ProviderErrorKind, ToolStatus, TurnEnd, Usage};

use crate::SharedWriter;

/// State that must be updated atomically with the write it describes.
struct Inner {
    /// True when the next stdout character begins a line (so it needs the prefix).
    line_start: bool,
    last_model: String,
    usage: UsageSums,
    /// Responses that completed. The host reads it to tell whether a turn that
    /// ended `ProviderFailed` made any progress (completion.md §3b).
    responses: u64,
}

/// Running sums of the usage fields, one unknown flag per part. Shared by the
/// parent's totals line and the workers' aggregate so that "sums of known parts,
/// unknown is `?`" has exactly one implementation.
#[derive(Debug, Default, Clone, Copy)]
struct UsageSums {
    /// Responses recorded. With none, nothing is known: a sum over no responses is
    /// not a measured zero (a run that never got an answer did not cost $0.0000).
    responses: u64,
    in_total: u64,
    in_unknown: bool,
    cached_total: u64,
    cached_unknown: bool,
    out_total: u64,
    out_unknown: bool,
    cost_total: u64,
    cost_unknown: bool,
}

impl UsageSums {
    /// Add one response. `None` means the response reported no usage at all, so
    /// every part becomes unknown.
    fn record(&mut self, usage: Option<Usage>) {
        self.responses += 1;
        match usage {
            None => {
                self.in_unknown = true;
                self.cached_unknown = true;
                self.out_unknown = true;
                self.cost_unknown = true;
            }
            Some(usage) => {
                match input_total(&usage) {
                    Some(total) => self.in_total += total,
                    None => self.in_unknown = true,
                }
                match usage.cache_read {
                    Some(value) => self.cached_total += value,
                    None => self.cached_unknown = true,
                }
                match usage.output {
                    Some(value) => self.out_total += value,
                    None => self.out_unknown = true,
                }
                match usage.cost_micro_usd {
                    Some(value) => self.cost_total += value,
                    None => self.cost_unknown = true,
                }
            }
        }
    }

    /// The four displayed parts, `?`/`unknown` for anything not known everywhere.
    fn parts(&self) -> (String, String, String, String) {
        if self.responses == 0 {
            let unknown = || "?".to_string();
            return (unknown(), unknown(), unknown(), "unknown".to_string());
        }
        (
            unknown_or(self.in_total, self.in_unknown),
            unknown_or(self.cached_total, self.cached_unknown),
            unknown_or(self.out_total, self.out_unknown),
            if self.cost_unknown {
                "unknown".to_string()
            } else {
                cost_string(Some(self.cost_total))
            },
        )
    }
}

fn unknown_or(value: u64, unknown: bool) -> String {
    if unknown {
        "?".to_string()
    } else {
        value.to_string()
    }
}

/// The aggregate the host prints after the parent's `total` line. ONE owner: the
/// host creates it and hands a clone to every child's renderer, which feeds it
/// each committed response. `workers` counts children that actually started, so a
/// run in which no worker ran prints no line at all.
#[cfg(feature = "delegation")]
#[derive(Debug, Default)]
pub(crate) struct WorkerUsage {
    inner: Mutex<WorkerUsageInner>,
}

#[cfg(feature = "delegation")]
#[derive(Debug, Default)]
struct WorkerUsageInner {
    workers: u64,
    sums: UsageSums,
}

#[cfg(feature = "delegation")]
impl WorkerUsage {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// One child was built and is running now.
    pub(crate) fn worker_started(&self) {
        self.inner.lock().unwrap().workers += 1;
    }

    /// One child response completed with this usage.
    pub(crate) fn record(&self, usage: Option<Usage>) {
        self.inner.lock().unwrap().sums.record(usage);
    }

    /// The line to print at exit, or `None` when no worker ran.
    pub(crate) fn line(&self) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        if inner.workers == 0 {
            return None;
        }
        let (input, cached, output, cost) = inner.sums.parts();
        Some(format!(
            "workers total ({}) · in {input} (cached {cached}) · out {output} · cost {cost}",
            inner.workers
        ))
    }
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
    /// Set on a child renderer so the run can print a workers aggregate.
    #[cfg(feature = "delegation")]
    worker_usage: Option<Arc<WorkerUsage>>,
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
            #[cfg(feature = "delegation")]
            worker_usage: None,
            inner: Mutex::new(Inner {
                line_start: true,
                last_model: String::new(),
                usage: UsageSums::default(),
                responses: 0,
            }),
        }
    }

    /// Feed every committed response of this renderer into `usage`. Used by the
    /// host on a child's renderer so the run can print the workers aggregate.
    #[cfg(feature = "delegation")]
    pub(crate) fn with_worker_usage(mut self, usage: Arc<WorkerUsage>) -> Self {
        self.worker_usage = Some(usage);
        self
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
        let (input, cached, output, cost) = inner.usage.parts();
        let line = format!(
            "total model {}/{model} · in {input} (cached {cached}) · out {output} · cost {cost}",
            self.route
        );
        self.write_line(&mut inner, true, &line);
    }

    /// How many provider responses this agent completed. The host reads it around
    /// a turn: a turn in which at least one response completed resets the count of
    /// consecutive transient provider failures (completion.md §3b).
    pub fn responses_completed(&self) -> u64 {
        self.inner.lock().unwrap().responses
    }

    /// One line per provider retry, naming the failure kind and the wait
    /// (completion.md §3b).
    pub fn provider_retry(
        &self,
        kind: ProviderErrorKind,
        retry: usize,
        max: usize,
        wait: Duration,
    ) {
        let mut inner = self.inner.lock().unwrap();
        self.close_line(&mut inner);
        let line = format!(
            "provider failed ({kind:?}): retry {retry}/{max} in {} s",
            wait.as_secs()
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
        inner.usage.record(usage);
        #[cfg(feature = "delegation")]
        if let Some(workers) = &self.worker_usage {
            workers.record(usage);
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
            // ADR-0048: display-only, so it goes to stderr like the other lines the
            // model's own text never mixes with — one line, nothing recorded.
            AgentEvent::ProviderNotice { text } => {
                self.close_line(&mut inner);
                self.write_line(&mut inner, true, &format!("· {text}"));
            }
            AgentEvent::ResponseCompleted { model, usage, .. } => {
                self.close_line(&mut inner);
                let line = usage_line(&self.route, &model, usage);
                self.write_line(&mut inner, true, &line);
                self.record_usage(&mut inner, usage);
                inner.last_model = model;
                inner.responses += 1;
            }
            AgentEvent::InboxDelivered { count } => {
                self.close_line(&mut inner);
                self.write_line(
                    &mut inner,
                    false,
                    &format!("• {count} notification(s) delivered"),
                );
            }
            AgentEvent::ContextReplaced {
                items_before,
                items_after,
                usage,
            } => {
                self.close_line(&mut inner);
                let model = if inner.last_model.is_empty() {
                    self.model.clone()
                } else {
                    inner.last_model.clone()
                };
                let line = context_line(&self.route, &model, items_before, items_after, usage);
                self.write_line(&mut inner, true, &line);
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

/// The line for a committed context replacement: the item counts, plus the usage
/// line when preparing reported a cost. Unknown usage is left off entirely rather
/// than printed as a made-up zero.
fn context_line(
    route: &str,
    model: &str,
    items_before: usize,
    items_after: usize,
    usage: Option<Usage>,
) -> String {
    let mut line = format!("context: summarized {items_before} → {items_after} items");
    if usage.is_some() {
        line.push_str(" · ");
        line.push_str(&usage_line(route, model, usage));
    }
    line
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

    /// A writer that keeps what it was given so a test can read it back.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn sums_over_no_responses_are_unknown_not_zero() {
        let (input, cached, output, cost) = UsageSums::default().parts();
        assert_eq!(
            (input.as_str(), cached.as_str(), output.as_str()),
            ("?", "?", "?")
        );
        assert_eq!(cost, "unknown");
    }

    #[test]
    fn renders_a_context_replacement_line_with_and_without_usage() {
        let stderr = Capture::default();
        let stdout: SharedWriter = Arc::new(Mutex::new(Box::new(Capture::default())));
        let stderr_writer: SharedWriter = Arc::new(Mutex::new(Box::new(stderr.clone())));
        let renderer = Renderer::new(
            stdout,
            stderr_writer,
            false,
            "r".into(),
            "m".into(),
            Arc::new(Mutex::new(String::new())),
        );
        let usage = Usage {
            input_uncached: Some(10),
            ..Usage::default()
        };
        renderer.emit(AgentEvent::ContextReplaced {
            items_before: 9,
            items_after: 3,
            usage: Some(usage),
        });
        renderer.emit(AgentEvent::ContextReplaced {
            items_before: 4,
            items_after: 4,
            usage: None,
        });
        assert_eq!(
            String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap(),
            "context: summarized 9 → 3 items · model r/m · in 10 (cached ?) · out ? · cost unknown\n\
             context: summarized 4 → 4 items\n"
        );
    }

    /// ADR-0048: a provider notice is one stderr line, `· ` and the adapter's own
    /// text. It is display only, so stdout — where the model's words go — stays
    /// untouched.
    #[test]
    fn renders_a_provider_notice_as_one_stderr_line() {
        let stdout = Capture::default();
        let stderr = Capture::default();
        let renderer = Renderer::new(
            Arc::new(Mutex::new(Box::new(stdout.clone()))),
            Arc::new(Mutex::new(Box::new(stderr.clone()))),
            false,
            "r".into(),
            "m".into(),
            Arc::new(Mutex::new(String::new())),
        );
        renderer.emit(AgentEvent::ProviderNotice {
            text: "transport: WebSocket unavailable (HTTP 500) — using HTTP (SSE) for the rest \
                   of this session"
                .into(),
        });
        assert_eq!(
            String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap(),
            "· transport: WebSocket unavailable (HTTP 500) — using HTTP (SSE) for the rest of \
             this session\n"
        );
        assert_eq!(
            String::from_utf8(stdout.0.lock().unwrap().clone()).unwrap(),
            ""
        );
    }

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
