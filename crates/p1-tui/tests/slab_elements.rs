//! Transcript element snapshots (handoff §6) against the SLAB oracle. Each mock is checked
//! twice: from hand-built blocks (every example), and through the real path — `AgentEvent`s
//! into `Transcript::apply` (or the host's transcript entry where no event exists) and then
//! the transcript renderer — so the event mapping is covered, not only the drawing.
mod common;

use common::slab;
use p1_contracts::{
    AgentEvent, ProviderError, ProviderErrorKind, StopReason, ToolCall, ToolInput, TurnEnd, Usage,
};
use p1_tui::render::{home, screen, transcript as render};
use p1_tui::state::Screen;
use p1_tui::transcript::{
    Block, CommandOutput, CommandRow, NoticeFact, NoticeKind, Transcript, TurnNotice, TurnPhase,
    TurnWorking, WorkerEnd, WorkerReport,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

/// Paint rendered rows into a buffer of exactly their size.
fn paint(lines: &[Line<'static>], width: u16) -> (Buffer, Rect) {
    let area = Rect::new(0, 0, width, lines.len() as u16);
    let mut buffer = Buffer::empty(area);
    for (y, line) in lines.iter().enumerate() {
        buffer.set_line(0, y as u16, line, width);
    }
    (buffer, area)
}

/// The whole transcript at `width`, reduced motion (the mocks cannot pin a `▪` pulse).
fn rows(t: &Transcript, width: u16, working: Option<&str>, now_ms: u64) -> Vec<Line<'static>> {
    render::lines(
        t,
        width as usize,
        usize::MAX,
        working.is_some(),
        now_ms,
        true,
    )
}

fn assert_whole(lines: &[Line<'static>], width: u16, id: &str) {
    let (buffer, area) = paint(lines, width);
    slab::assert_mock(&buffer, area, id);
}

/// One rendered row against one mock row.
fn assert_row(line: &Line<'static>, width: u16, id: &str, mock_row: usize) {
    let (buffer, area) = paint(std::slice::from_ref(line), width);
    slab::assert_mock_rows(&buffer, area, id, mock_row..mock_row + 1);
}

fn stamp(t: &mut Transcript, at_ms: u64, event: AgentEvent) {
    t.apply(&event, Some(at_ms));
}

const LONG_PROMPT: &str = "the compaction boundary stalls when the worker already has a summary ready; find where the hard-pressure wait blocks and fix it without changing the summary format";
const STEERING: &str = "use the existing apply_at_boundary helper";

// ---------- OperatorTurn (§6.2) ----------

#[test]
fn el_operator_every_example() {
    let mut t = Transcript::new();
    t.blocks.push(Block::Operator {
        text: LONG_PROMPT.into(),
        steering: false,
    });
    t.blocks.push(Block::Operator {
        text: STEERING.into(),
        steering: true,
    });
    assert_whole(&rows(&t, 76, None, 0), 76, "el-operator@76");
    assert_whole(&rows(&t, 56, None, 0), 56, "el-operator@56");
}

#[test]
fn el_operator_real_path_steering_is_delivered_at_the_inbox_boundary() {
    let mut t = Transcript::new();
    t.operator(LONG_PROMPT);
    t.queue_steering(STEERING);
    stamp(&mut t, 1_000, AgentEvent::InboxDelivered { count: 1 });
    assert_whole(&rows(&t, 76, None, 0), 76, "el-operator@76");
    assert_whole(&rows(&t, 56, None, 0), 56, "el-operator@56");
}

// ---------- ProseFlow (§6.3) ----------

const PROSE: &str = "The hard-pressure wait in `crates/p1-context/src/edge.rs` blocks the turn boundary instead of applying the summary the worker already prepared. Three things line up:";

#[test]
fn prose_real_path_a_backticked_path_that_exists_is_a_ref() {
    let mut t = Transcript::new();
    t.set_path_exists(|path| path == "crates/p1-context/src/edge.rs");
    stamp(&mut t, 0, AgentEvent::TextDelta { text: PROSE.into() });
    let (buffer, area) = paint(&rows(&t, 76, None, 0), 76);
    slab::assert_mock_region(&buffer, area, "E01", 5, 2);
}

#[test]
fn prose_keeps_backticks_verbatim_unless_the_path_exists() {
    let mut t = Transcript::new();
    stamp(&mut t, 0, AgentEvent::TextDelta { text: PROSE.into() });
    let text: String = rows(&t, 120, None, 0)[0].to_string();
    assert!(
        text.contains("`crates/p1-context/src/edge.rs`"),
        "the default check knows no path: {text}"
    );
}

// ---------- Reasoning (§6.4) ----------

const REASONING: &str = "The wait only exists for the no-summary case. If a summary is ready the boundary can apply it directly; the hard-pressure branch predates the worker summary path.";

#[test]
fn el_reasoning_every_example() {
    let mut t = Transcript::new();
    t.blocks.push(Block::Reasoning {
        lines: vec!["weighing".into()],
        expanded: false,
        elapsed_ms: Some(4_200),
        started_ms: None,
    });
    t.blocks.push(Block::Reasoning {
        lines: vec![REASONING.into()],
        expanded: true,
        elapsed_ms: Some(4_200),
        started_ms: None,
    });
    assert_whole(&rows(&t, 76, None, 0), 76, "el-reasoning@76");
}

#[test]
fn el_reasoning_real_path_measures_from_the_first_delta_to_the_next_event() {
    let completed = || AgentEvent::ResponseCompleted {
        model: "m".into(),
        stop: StopReason::ToolUse,
        usage: None,
    };
    let mut t = Transcript::new();
    stamp(
        &mut t,
        1_000,
        AgentEvent::ReasoningDelta {
            text: "weighing".into(),
        },
    );
    stamp(&mut t, 5_200, completed());
    stamp(
        &mut t,
        6_000,
        AgentEvent::ReasoningDelta {
            text: REASONING.into(),
        },
    );
    stamp(&mut t, 10_200, completed());
    t.toggle_reasoning();
    assert_whole(&rows(&t, 76, None, 20_000), 76, "el-reasoning@76");
}

#[test]
fn streaming_reasoning_counts_live() {
    let mut t = Transcript::new();
    stamp(
        &mut t,
        1_000,
        AgentEvent::ReasoningDelta { text: "why".into() },
    );
    let at = |now_ms| rows(&t, 76, None, now_ms)[0].to_string();
    assert!(at(3_100).contains("· reasoning 2.1s"), "{}", at(3_100));
    assert!(at(4_250).contains("· reasoning 3.2s"), "{}", at(4_250));
}

// ---------- TurnWorking (§6.5) ----------

#[test]
fn el_working_every_example() {
    let examples = [
        (TurnPhase::Waiting, 1_200, 1),
        (TurnPhase::Reasoning, 3_000, 1),
        (TurnPhase::Streaming, 6_000, 3),
        (TurnPhase::Preparing, 2_400, 3),
        (TurnPhase::Summarizing, 8_100, 9),
    ];
    let mut lines = Vec::new();
    for (n, (phase, elapsed_ms, request)) in examples.into_iter().enumerate() {
        if n > 0 {
            lines.push(Line::default());
        }
        let facts = TurnWorking {
            phase,
            elapsed_ms: Some(elapsed_ms),
            request: Some(request),
        };
        lines.push(render::turn_working_line(&facts, 76, 0, true));
    }
    assert_whole(&lines, 76, "el-working@76");
}

#[test]
fn el_working_real_path_follows_the_turn_events() {
    let started = |t: &mut Transcript, index: u32| {
        stamp(t, 0, AgentEvent::TurnStarted);
        stamp(
            t,
            10,
            AgentEvent::RequestStarted {
                request_index: index,
            },
        );
    };
    let last = |t: &Transcript, now_ms| rows(t, 76, Some(""), now_ms).pop().unwrap();

    let mut t = Transcript::new();
    started(&mut t, 0);
    assert_row(&last(&t, 1_200), 76, "el-working@76", 0);

    let mut t = Transcript::new();
    started(&mut t, 0);
    stamp(
        &mut t,
        500,
        AgentEvent::ReasoningDelta { text: "why".into() },
    );
    assert_row(&last(&t, 3_000), 76, "el-working@76", 2);

    let mut t = Transcript::new();
    started(&mut t, 2);
    stamp(&mut t, 500, AgentEvent::TextDelta { text: "The".into() });
    assert_row(&last(&t, 6_000), 76, "el-working@76", 4);

    let mut t = Transcript::new();
    started(&mut t, 2);
    stamp(
        &mut t,
        500,
        AgentEvent::ToolInputDelta {
            call_id: "c1".into(),
            text: "*** Begin Patch".into(),
        },
    );
    assert_row(&last(&t, 2_400), 76, "el-working@76", 6);
    // The row is separated from the element above it by one blank row.
    let all = rows(&t, 76, Some(""), 2_400);
    assert_eq!(all[all.len() - 2], Line::default());
}

#[test]
fn a_running_block_carries_the_working_cells_instead_of_the_turn_row() {
    let mut s = Screen::new(true);
    s.apply(&AgentEvent::TurnStarted, 0);
    s.apply(
        &AgentEvent::ToolStarted {
            call: ToolCall {
                call_id: "c1".into(),
                name: "shell".into(),
                input: ToolInput::Json("cargo test -p p1-context boundary".into()),
            },
        },
        1_000,
    );
    let header = |now_ms| {
        let lines = rows(&s.transcript, 76, Some("shell"), now_ms);
        assert_eq!(lines.len(), 1, "no turn working row while a Block runs");
        lines[0].to_string()
    };
    // Elapsed = now − the call's start stamp, whole tenths, every frame.
    assert!(header(5_200).ends_with("4.2s  ▪▪▪  "), "{}", header(5_200));
    assert!(
        header(12_450).ends_with("11.4s  ▪▪▪  "),
        "{}",
        header(12_450)
    );
}

// ---------- MetaRow (§6.6) ----------

const NOTICE: &str =
    "transport: WebSocket unavailable (426) — using HTTP (SSE) for the rest of this session";
const SWITCH: &str = "· model claude/opus-5.5:high → gpt/gpt-5.6-sol:medium · from the next turn";

#[test]
fn el_meta_every_example() {
    let mut t = Transcript::new();
    let meta = |text: &str| Block::Meta { text: text.into() };
    t.blocks.push(meta(&format!("· {NOTICE}")));
    t.blocks.push(Block::MetaFacts {
        text: "· context summarized · 214 → 31 items".into(),
        facts: "in 18.2k · out 1.1k".into(),
    });
    t.blocks.push(meta(SWITCH));
    t.blocks.push(meta("· goal set"));
    t.blocks.push(meta("· 1 inbox message delivered"));
    t.blocks.push(meta("· /env is not a command · /help"));
    t.blocks.push(meta("· 2 workers not restored on resume"));
    assert_whole(&rows(&t, 76, None, 0), 76, "el-meta@76");
}

#[test]
fn el_meta_real_path_maps_notices_replacements_and_inbox_deliveries() {
    let mut t = Transcript::new();
    stamp(
        &mut t,
        0,
        AgentEvent::ProviderNotice {
            text: NOTICE.into(),
        },
    );
    stamp(
        &mut t,
        10,
        AgentEvent::ContextReplaced {
            items_before: 214,
            items_after: 31,
            usage: Some(Usage {
                input_uncached: Some(18_200),
                output: Some(1_100),
                ..Usage::default()
            }),
        },
    );
    t.note(SWITCH);
    t.note("· goal set");
    stamp(&mut t, 20, AgentEvent::InboxDelivered { count: 1 });
    t.note("· /env is not a command · /help");
    t.note("· 2 workers not restored on resume");
    assert_whole(&rows(&t, 76, None, 0), 76, "el-meta@76");
}

// ---------- TurnNotice (§6.7, §7.8) ----------

fn notice(kind: NoticeKind, headline: &str, facts: &[(NoticeFact, &str)]) -> Block {
    Block::Notice(TurnNotice {
        kind,
        headline: headline.into(),
        facts: facts.iter().map(|(f, v)| (*f, v.to_string())).collect(),
    })
}

#[test]
fn el_endings_every_example() {
    use NoticeFact::{Cost, Kept, Next, Reason};
    use NoticeKind::{Failed, Stopped};
    let mut t = Transcript::new();
    t.blocks = vec![
        notice(
            Stopped,
            "cancelled at 12.4s",
            &[
                (Cost, "request 3 · in 14.2k · out 0.4k · —"),
                (
                    Kept,
                    "journal · shell settled as cancelled · dropped 1 queued",
                ),
            ],
        ),
        notice(
            Failed,
            "rate limited · glm/5.3 · HTTP 429",
            &[
                (Cost, "request 7 · in 22.9k · —"),
                (Kept, "journal · 3 files changed · w2 running"),
                (Next, "wait for the window, or /model"),
            ],
        ),
        notice(
            Failed,
            "authentication failed · claude (anthropic-subscription)",
            &[
                (Kept, "journal"),
                (Next, "sign in to Claude Code again · p1 login --list"),
            ],
        ),
        notice(
            Failed,
            "account exhausted · deepseek2 (opencode-go-2-subscription) · not retried",
            &[
                (Kept, "journal · 1 file changed"),
                (Next, "/model to continue on another route"),
            ],
        ),
        notice(
            Failed,
            "connection failed · Transport: chat stream ended before [DONE]",
            &[
                (Cost, "request 14 · 1.2k out streamed, not kept"),
                (Kept, "journal"),
                (Next, "send again to continue · /model"),
            ],
        ),
        notice(
            Failed,
            "journal commit failed · No space left on device (os error 28)",
            &[
                (Kept, "nothing after the last committed record happened"),
                (Next, "free space, then restart p1 to resume"),
            ],
        ),
        notice(
            Failed,
            "context failed · at the wall: 118k of 120k",
            &[
                (Cost, "summary request · in 96.4k · —"),
                (Kept, "history unchanged · journal"),
                (Next, "/model to a larger window"),
            ],
        ),
        notice(
            Stopped,
            "stopped · max output tokens",
            &[(Cost, "in 12.1k · out 32k · —")],
        ),
        notice(
            Failed,
            "switch refused · gpt/gpt-5.6-sol",
            &[
                (Reason, "the history holds a call this route cannot carry"),
                (Kept, "still on claude/opus-5.5:high"),
            ],
        ),
    ];
    assert_whole(&rows(&t, 76, None, 0), 76, "el-endings@76");
}

#[test]
fn el_endings_real_path_a_commit_failure() {
    let mut t = Transcript::new();
    stamp(&mut t, 0, AgentEvent::TurnStarted);
    stamp(
        &mut t,
        900,
        AgentEvent::TurnFinished {
            end: TurnEnd::CommitFailed {
                message: "No space left on device (os error 28)".into(),
            },
        },
    );
    let (buffer, area) = paint(&rows(&t, 76, None, 0), 76);
    slab::assert_mock_rows(&buffer, area, "el-endings@76", 23..26);
}

#[test]
fn el_endings_real_path_a_cancel_settles_the_running_call_and_drops_the_queue() {
    let mut t = Transcript::new();
    stamp(&mut t, 0, AgentEvent::TurnStarted);
    for index in 0..3 {
        stamp(
            &mut t,
            100 + u64::from(index),
            AgentEvent::RequestStarted {
                request_index: index,
            },
        );
    }
    stamp(
        &mut t,
        4_000,
        AgentEvent::ToolStarted {
            call: ToolCall {
                call_id: "c1".into(),
                name: "shell".into(),
                input: ToolInput::Json("cargo test -p p1-context boundary".into()),
            },
        },
    );
    t.queue_steering("also check the wrap boundary");
    stamp(
        &mut t,
        12_400,
        AgentEvent::TurnFinished {
            end: TurnEnd::Cancelled,
        },
    );
    let lines = rows(&t, 76, None, 12_400);
    let notice = &lines[lines.len() - 3..];
    assert_row(&notice[0], 76, "el-endings@76", 0);
    assert_row(&notice[2], 76, "el-endings@76", 2);
    assert!(
        lines[0].to_string().contains("· cancelled"),
        "the running call settles cancelled: {}",
        lines[0]
    );
}

#[test]
fn a_transport_failure_names_what_broke_and_what_to_do() {
    let mut t = Transcript::new();
    stamp(
        &mut t,
        0,
        AgentEvent::TurnFinished {
            end: TurnEnd::ProviderFailed {
                error: ProviderError::new(
                    ProviderErrorKind::Transport,
                    "chat stream ended before [DONE]",
                ),
            },
        },
    );
    // Without a turn there is no cost to state; every other row is the mock's.
    let lines = rows(&t, 76, None, 0);
    assert_eq!(lines.len(), 3);
    assert_row(&lines[0], 76, "el-endings@76", 18);
    assert_row(&lines[1], 76, "el-endings@76", 20);
    assert_row(&lines[2], 76, "el-endings@76", 21);
}

// ---------- WorkerReport (§6.8) ----------

fn reports() -> Vec<WorkerReport> {
    let report =
        |id: &str, route: &str, end, elapsed: &str, grants: &[&str], line: &str| WorkerReport {
            id: id.into(),
            route: route.into(),
            end,
            elapsed: Some(elapsed.into()),
            cost_micro_usd: None,
            grants: grants.iter().map(|g| g.to_string()).collect(),
            line: line.into(),
        };
    vec![
        report(
            "w2",
            "deepseek2/v4.1-flash",
            WorkerEnd::Done,
            "2m10s",
            &["read", "edit", "shell", "finish"],
            "done · verified · cargo test -p p1-provider-http",
        ),
        report(
            "w1",
            "claude/sonnet-5",
            WorkerEnd::Blocked,
            "0m41s",
            &["read", "grep", "finish"],
            "blocked: needs edit — tried edit ×2",
        ),
        report(
            "w5",
            "gpt/gpt-5.6-luna",
            WorkerEnd::NotVerified,
            "1m12s",
            &["read", "edit", "finish"],
            "done · not verified — parent verification required",
        ),
        report(
            "w4",
            "glm/5.3",
            WorkerEnd::Failed,
            "1m03s",
            &["read", "shell", "finish"],
            "failed: RateLimited · HTTP 429",
        ),
        report(
            "w6",
            "deepseek/v4.1-flash",
            WorkerEnd::Stalled,
            "6m40s",
            &["read", "edit", "finish"],
            "stalled: 6 summaries without a workspace change",
        ),
    ]
}

#[test]
fn el_worker_report_every_example() {
    let mut t = Transcript::new();
    t.blocks = reports().into_iter().map(Block::WorkerReport).collect();
    assert_whole(&rows(&t, 76, None, 0), 76, "el-worker-report@76");
}

#[test]
fn el_worker_report_real_path_the_hosts_entry() {
    // No AgentEvent reports a worker's end: the host calls the transcript directly (§7.7).
    let mut t = Transcript::new();
    for report in reports() {
        t.worker_report(report);
    }
    assert_whole(&rows(&t, 76, None, 0), 76, "el-worker-report@76");
}

// ---------- CommandOutput (§6.9) ----------

#[test]
fn command_output_real_path_matches_the_help_screen() {
    let entry = |key: &str, text: &str| CommandRow::Entry {
        key: key.into(),
        text: text.into(),
    };
    let mut t = Transcript::new();
    stamp(
        &mut t,
        0,
        AgentEvent::TextDelta {
            text:
                "Confirmed — the ready summary never applies. Fixing the boundary and re-running."
                    .into(),
        },
    );
    t.command_output(CommandOutput {
        command: "/help".into(),
        argument: String::new(),
        facts: "10 commands · 14 keys".into(),
        body: vec![
            CommandRow::Head("COMMANDS".into()),
            entry(
                "/model [REF]",
                "switch model or effort · claude/opus-5.5:high",
            ),
            entry("/effort LEVEL", "low medium high max"),
            entry(
                "/goal [TEXT]",
                "set or clear the session objective · ^G edits",
            ),
            entry("/focus [on|off]", "transcript only"),
            entry("/status", "session facts"),
            entry("/resume", "reopen a previous session"),
            entry("/access", "access and sandbox (fixed per process)"),
            entry("/models", "every model p1 can run"),
            entry("/exit", "quit"),
            CommandRow::Head("KEYS".into()),
            entry("⏎  ⌥⏎", "send · newline; while working: steer · follow-up"),
            entry("^C", "cancel the turn · quit when idle"),
            entry("^O  ^R", "open the latest fold · toggle reasoning"),
            entry("^Tab ^N  ^W  ^P", "pane mode · width · pin"),
            entry("^F  ^L", "focus the pane · pane overlay under 100 cols"),
            entry("^G  PgUp PgDn  esc", "goal · scroll · live tail / dismiss"),
        ],
    });
    let (buffer, _) = paint(&rows(&t, 76, None, 0), 76);
    // All mock cells except the repaired boundary remain pinned. The `esc` key occupies
    // 18 cells; assert the two-cell gap, then the unchanged description and row remainder.
    slab::assert_mock_region(&buffer, Rect::new(0, 0, 76, 20), "C04", 1, 2);
    let expected = slab::mock("C04").text[21][2..78].to_string();
    let actual = buffer.area;
    let rendered: String = (0..actual.width)
        .map(|x| buffer[(x, 20)].symbol())
        .collect();
    let key = "^G  PgUp PgDn  esc";
    let key_start = expected.find(key).unwrap();
    let description_start = key_start + key.len();
    assert_eq!(p1_tui::wrap::cell_width(key), 18);
    assert_eq!(
        p1_tui::wrap::cell_width(&rendered[key_start..description_start]),
        18
    );
    assert!(rendered[key_start + 18..].starts_with("  goal · scroll · live tail / dismiss"));
    let description = "goal · scroll · live tail / dismiss";
    assert!(rendered[key_start + 20..].starts_with(description));
    assert_eq!(
        &rendered[key_start + 20..key_start + 20 + description.len()],
        description,
        "the description remains unchanged after the gap"
    );
}

// ---------- Monogram (§6.11) ----------

#[test]
fn el_monogram_every_example() {
    let area = Rect::new(0, 0, 76, 14);
    let mut buffer = Buffer::empty(area);
    home::draw(area, &mut buffer);
    slab::assert_mock_rows(&buffer, Rect::new(0, 0, 76, 13), "el-monogram@76", 0..13);
}

#[test]
fn el_monogram_real_path_the_empty_screen() {
    let mut s = Screen::new(true);
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    screen::draw(&mut s, area, &mut buffer, 0);
    // 80×24: the transcript is rows 0–19 at column 2; the 13-row element is centred in it.
    slab::assert_mock_rows(&buffer, Rect::new(2, 3, 76, 13), "el-monogram@76", 0..13);
}

#[test]
fn the_monogram_needs_a_30_by_14_free_area() {
    for (width, height) in [(76, 13), (29, 20)] {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        home::draw(area, &mut buffer);
        assert_eq!(
            buffer,
            Buffer::empty(area),
            "{width}×{height}: no mark, no words"
        );
    }
}
