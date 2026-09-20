//! Render fixture screens through the real composition and dump them as ANSI
//! truecolor to stdout, so the visual loop can run in a real terminal:
//! `cargo run -p p1-tui --example preview -- <screen>` inside tmux at 120x40.
//! Screens: streaming (default), idle, fold, diff, permission, picker, failure.

use p1_contracts::{AgentEvent, ToolCall, ToolInput, ToolResultItem, ToolStatus};
use p1_tui::render::diff::{DiffRow, DiffView};
use p1_tui::render::ledger::{Context, ContextPart, SpendView, Task};
use p1_tui::render::permission::PermissionView;
use p1_tui::render::picker::{Picker, PickerGroup, PickerRow};
use p1_tui::render::screen::draw;
use p1_tui::state::{Approval, Screen};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

fn tool(name: &str, id: &str, input: &str) -> AgentEvent {
    AgentEvent::ToolStarted {
        call: ToolCall {
            call_id: id.into(),
            name: name.into(),
            input: ToolInput::Json(input.into()),
        },
    }
}

fn done(id: &str, name: &str, status: ToolStatus, content: &str) -> AgentEvent {
    AgentEvent::ToolFinished {
        result: ToolResultItem {
            call_id: id.into(),
            name: name.into(),
            status,
            content: content.into(),
        },
    }
}

fn streaming() -> Screen {
    let mut s = Screen::new(false);
    s.goal = Some("fix compaction boundary stall".into());
    s.transcript.operator("why does compaction stall at the turn edge?");
    s.apply(
        &AgentEvent::ReasoningDelta {
            text: "the hard-pressure wait blocks the boundary".into(),
        },
        0,
    );
    s.apply(
        &AgentEvent::ResponseCompleted {
            model: "sonnet-4.5".into(),
            stop: p1_contracts::StopReason::EndTurn,
            usage: Some(p1_contracts::Usage {
                input_uncached: Some(32_000),
                cache_read: Some(6_100),
                output: Some(1_900),
                ..Default::default()
            }),
        },
        4_200,
    );
    s.apply(
        &AgentEvent::TextDelta {
            text: "The hard-pressure wait in `p1-context` blocks the turn boundary instead of applying the summary the worker already prepared. Three things line up:".into(),
        },
        4_300,
    );
    s.apply(&tool("read", "c1", "p1-context/src/edge.rs"), 5_100);
    s.apply(&done("c1", "read", ToolStatus::Ok, &"x\n".repeat(412)), 5_300);
    s.apply(&tool("search", "c2", "block_until_ready"), 5_400);
    s.apply(&done("c2", "search", ToolStatus::Ok, "a\nb\nc"), 5_600);
    s.apply(&tool("shell", "c3", "cargo test -p p1-context boundary"), 5_700);
    s
}

fn idle() -> Screen {
    let mut s = Screen::new(true);
    s.transcript.blocks.push(p1_tui::transcript::Block::Info {
        lines: vec![
            "p1 0.1.0   ~/dev/phaseone   main".into(),
            String::new(),
            "  no journal in this directory.".into(),
            String::new(),
            "  /resume     reopen a previous session".into(),
            "  /env        claude · sonnet-4.5".into(),
            "  /access     full · --ask to confirm".into(),
            "  /goal       set the session objective".into(),
        ],
    });
    s
}

fn fold() -> Screen {
    let mut s = Screen::new(true);
    s.apply(&tool("read", "c1", "p1-context/src/edge.rs"), 0);
    s.apply(&done("c1", "read", ToolStatus::Ok, &"x\n".repeat(412)), 100);
    s.apply(&tool("search", "c2", "block_until_ready"), 200);
    s.apply(&done("c2", "search", ToolStatus::Ok, "a\nb\nc"), 300);
    s.apply(&tool("shell", "c3", "cargo test -p p1-context boundary"), 400);
    let big: String = (0..94)
        .map(|n| {
            if n == 3 {
                "test compaction::hard_pressure_waits ... FAILED".to_string()
            } else {
                format!("test compaction::case_{n} ... ok")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    s.apply(&done("c3", "shell", ToolStatus::Error, &big), 11_400);
    s.transcript.apply(
        &AgentEvent::TextDelta {
            text: "Confirmed — the ready summary never applies.".into(),
        },
        None,
    );
    s
}

fn diff() -> Screen {
    let mut s = Screen::new(true);
    s.approval = Some(Approval::Diff(DiffView {
        tool: "edit".into(),
        file: "p1-context/src/edge.rs".into(),
        summary: "replace exact string · once".into(),
        position: (1, 3),
        rows: vec![
            DiffRow::Context { line: 410, text: "let ready = worker.take_summary();".into() },
            DiffRow::Context { line: 411, text: "let pressure = self.pressure_at_edge();".into() },
            DiffRow::Del { line: 412, text: "if pressure == Pressure::Hard {".into() },
            DiffRow::Del { line: 413, text: "    block_until_ready(&worker);".into() },
            DiffRow::Del { line: 414, text: "}".into() },
            DiffRow::Add { line: 412, text: "if let Some(summary) = ready {".into() },
            DiffRow::Add { line: 413, text: "    return self.apply_at_boundary(summary);".into() },
            DiffRow::Add { line: 414, text: "}".into() },
        ],
        grantable: true,
    }));
    s
}

fn permission() -> Screen {
    let mut s = Screen::new(true);
    s.approval = Some(Approval::Permission(PermissionView {
        command: "rm -rf target/".into(),
        rows: vec![
            ("cwd".into(), "~/dev/phaseone".into()),
            ("sandbox".into(), "bubblewrap · writes: workspace".into()),
            ("network".into(), "off".into()),
            ("reason".into(), "destructive floor".into()),
        ],
        grantable: false,
    }));
    s
}

fn picker() -> Screen {
    let mut s = Screen::new(true);
    s.picker = Some(Picker {
        groups: vec![
            PickerGroup {
                header: "ANTHROPIC ROUTE".into(),
                rows: vec![
                    PickerRow { label: "claude · sonnet-4.5".into(), value: "300k · $3/$15".into(), available: true },
                    PickerRow { label: "claude · opus-4.8".into(), value: "300k · $15/$75".into(), available: true },
                ],
            },
            PickerGroup {
                header: "OPENAI-CHAT ROUTE".into(),
                rows: vec![
                    PickerRow { label: "deepseek · v4.1-flash".into(), value: "128k · $0.14/$0.28".into(), available: true },
                    PickerRow { label: "glm · 5.3".into(), value: "quota exhausted".into(), available: false },
                ],
            },
        ],
        filter: String::new(),
        selected: 1,
    });
    s
}

fn main() {
    let name = std::env::args().nth(1).unwrap_or_else(|| "streaming".into());
    let mut screen = match name.as_str() {
        "idle" => idle(),
        "fold" => fold(),
        "diff" => diff(),
        "permission" => permission(),
        "picker" => picker(),
        _ => streaming(),
    };
    if name == "streaming" {
        // Fill the ledger like §5's example.
        screen.goal = Some("fix compaction boundary stall".into());
        screen.context_view = Some(Context {
            used: 12_400,
            window: 200_000,
            warn_at: 120_000,
            parts: vec![
                ContextPart { label: "system".into(), count: None, tokens: 1_200 },
                ContextPart { label: "files".into(), count: Some(4), tokens: 6_800 },
                ContextPart { label: "tools".into(), count: Some(11), tokens: 3_100 },
                ContextPart { label: "recent".into(), count: None, tokens: 1_300 },
            ],
        });
        screen.task_view = Some(Task {
            id: "t-3f9a".into(),
            files: Some(3),
            diff: Some((48, 12)),
            journal: Some("2m ago".into()),
        });
        let _ = SpendView::default();
    }
    let (w, h) = (120u16, 40u16);
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw(&screen, frame.area(), frame.buffer_mut(), 900))
        .unwrap();
    let buf = terminal.backend().buffer().clone();

    // Dump the buffer as ANSI truecolor rows.
    let mut out = String::new();
    let mut last_fg = None;
    let mut last_bg = None;
    for y in 0..h {
        for x in 0..w {
            let cell = &buf[(x, y)];
            let fg = Some(cell.fg);
            let bg = Some(cell.bg);
            if fg != last_fg {
                out.push_str(&ansi_fg(cell.fg));
                last_fg = fg;
            }
            if bg != last_bg {
                out.push_str(&ansi_bg(cell.bg));
                last_bg = bg;
            }
            out.push_str(cell.symbol());
        }
        out.push_str("\x1b[0m\n");
        last_fg = None;
        last_bg = None;
    }
    print!("{out}");
}

fn ansi_fg(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("\x1b[38;2;{r};{g};{b}m"),
        _ => "\x1b[39m".into(),
    }
}

fn ansi_bg(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("\x1b[48;2;{r};{g};{b}m"),
        _ => "\x1b[49m".into(),
    }
}
