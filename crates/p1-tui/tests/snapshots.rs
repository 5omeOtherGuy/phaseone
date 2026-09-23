//! Snapshot tests: handoff geometry at 120x40 and 80x24, rendered through
//! the real composition into a `TestBackend` cell buffer. Global rules check
//! the closed §2 token palette and glyph-readable state.

use p1_contracts::{AgentEvent, ToolCall, ToolInput, ToolResultItem, ToolStatus};
use p1_tui::render::diff::{DiffRow, DiffView};
use p1_tui::render::permission::PermissionView;
use p1_tui::render::picker::{Picker, PickerGroup, PickerRow};
use p1_tui::render::screen::draw;
use p1_tui::state::{Approval, PaneWidth, Screen};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

/// Render one screen and return its cell rows, trailing spaces trimmed.
fn render(screen: &mut Screen, width: u16, height: u16, now_ms: u64) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw(screen, frame.area(), frame.buffer_mut(), now_ms))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// Crop each row to the transcript column, so transcript assertions ignore
/// whatever the right pane is showing.
fn left(text: &[String], cols: usize) -> Vec<String> {
    text.iter()
        .map(|l| {
            l.chars()
                .skip(2)
                .take(cols)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// Every colour used anywhere on the screen, for the closed-palette law.
fn colors_used(screen: &mut Screen, width: u16, height: u16) -> Vec<Color> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw(screen, frame.area(), frame.buffer_mut(), 0))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut colors: Vec<Color> = Vec::new();
    for cell in buffer.content() {
        for color in [cell.fg, cell.bg] {
            if !colors.contains(&color) {
                colors.push(color);
            }
        }
    }
    colors
}

/// Independent copy of the handoff §2 24-bit token colors.
const ALLOWED: &[Color] = &[
    Color::Rgb(0x0a, 0x0a, 0x0a), // GROUND
    Color::Rgb(0x12, 0x12, 0x12), // BLOCK
    Color::Rgb(0x1c, 0x1c, 0x1c), // BLOCK+
    Color::Rgb(0x2a, 0x2a, 0x2a), // RULE
    Color::Rgb(0xe8, 0xe8, 0xe8), // INK
    Color::Rgb(0x9a, 0x9a, 0x9a), // DIM
    Color::Rgb(0x6a, 0x6a, 0x6a), // FAINT
    Color::Rgb(0xe2, 0xa0, 0x3f), // ATTN / amber fill
    Color::Rgb(0xe0, 0x70, 0x5f), // FAIL
    Color::Rgb(0x8f, 0xb5, 0x73), // OK
    Color::Rgb(0x72, 0xb8, 0xb0), // LIVE
    Color::Rgb(0x7f, 0xa7, 0xd6), // REF
    Color::Rgb(0xc0, 0x8f, 0xc8), // SYNTAX
    Color::Rgb(0x3a, 0x4a, 0x3a), // DIFF_ADD_BG
    Color::Rgb(0xd8, 0xe8, 0xd0), // DIFF_ADD_FG
    Color::Rgb(0x4a, 0x35, 0x35), // DIFF_DEL_BG
    Color::Rgb(0xe8, 0xd0, 0xd0), // DIFF_DEL_FG
    Color::Reset,                 // Unstyled blank-cell foreground.
];

fn assert_palette_law(screen: &mut Screen, width: u16, height: u16) {
    let used = colors_used(screen, width, height);
    for color in &used {
        assert!(
            ALLOWED.contains(color),
            "colour outside the palette: {color:?}"
        );
    }
}

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

/// §4.2 fixture: a turn mid-flight — prose, collapsed reasoning, two settled
/// calls, one running.
fn streaming_screen() -> Screen {
    let mut s = Screen::new(true);
    s.env = "ask".into();
    s.route = "claude".into();
    s.transcript
        .operator("why does compaction stall at the turn edge?");
    s.apply(
        &AgentEvent::ReasoningDelta {
            text: "the hard-pressure wait blocks the boundary".into(),
        },
        0,
    );
    s.apply(
        &AgentEvent::ResponseCompleted {
            model: "m".into(),
            stop: p1_contracts::StopReason::EndTurn,
            usage: None,
        },
        4_200,
    );
    s.apply(&AgentEvent::TextDelta { text: "The hard-pressure wait in `p1-context` blocks the turn boundary instead of applying the summary.".into() }, 4_300);
    s.apply(
        &AgentEvent::ResponseCompleted {
            model: "m".into(),
            stop: p1_contracts::StopReason::EndTurn,
            usage: None,
        },
        5_000,
    );
    s.apply(&tool("read", "c1", "p1-context/src/edge.rs"), 5_100);
    s.apply(
        &done("c1", "read", ToolStatus::Ok, &"x\n".repeat(412)),
        5_300,
    );
    s.apply(
        &tool("shell", "c2", "cargo test -p p1-context boundary"),
        5_400,
    );
    s
}

#[test]
fn streaming_at_120x40() {
    let mut s = streaming_screen();
    s.statusbar.model = Some("claude/sonnet-4.5".into());
    s.statusbar.ctx = Some("—".into());
    let screen = render(&mut s, 120, 40, 11_400);
    let text = left(&screen, 80);
    assert_palette_law(&mut s, 120, 40);
    assert_eq!(text[1], "› why does compaction stall at the turn edge?");
    assert!(text[2].starts_with("· reasoning"));
    assert!(text[2].ends_with("^R expand"));
    assert!(text[3].starts_with("The hard-pressure wait"));
    assert!(text[5].starts_with("✓ read      p1-context/src/edge.rs"));
    assert!(text[5].ends_with("412 lines"));
    assert_eq!(text[6], "▸ shell     cargo test -p p1-context boundary");
    assert!(text[7].starts_with("▪▪▪ shell"));
    // The inset transcript, composer and statusline occupy the §4 geometry rows.
    let layout = p1_tui::geometry::layout(120, 40, s.pane_width, false, 2);
    assert!(
        text[layout.composer.y as usize..layout.composer.bottom() as usize]
            .iter()
            .any(|row| row.contains('›'))
    );
    assert!(screen[layout.statusline.y as usize].contains("claude/sonnet-4.5"));
    assert!(screen[layout.statusline.y as usize].contains("ctx"));
    assert_eq!(text[39], "");
}

#[test]
fn streaming_at_80x24_collapses_the_pane() {
    let mut s = streaming_screen();
    let text = render(&mut s, 80, 24, 11_400);
    assert_palette_law(&mut s, 80, 24);
    // Newest rows stay visible; the statusline replaces the old floor line.
    let layout = p1_tui::geometry::layout(80, 24, s.pane_width, false, 2);
    assert!(
        text[layout.composer.y as usize..layout.composer.bottom() as usize]
            .iter()
            .any(|row| row.contains("›"))
    );
    assert!(
        text[layout.composer.y as usize..layout.composer.bottom() as usize]
            .iter()
            .any(|row| row.contains("⏎ queue steering"))
    );
    s.statusbar.model = Some("claude/sonnet-4.5".into());
    s.statusbar.ctx = Some("—".into());
    let text = render(&mut s, 80, 24, 11_400);
    assert!(text[layout.statusline.y as usize].contains("claude/sonnet-4.5"));
    assert!(text[layout.statusline.y as usize].contains("ctx"));
}

#[test]
fn idle_screen_affordances() {
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
    let text = left(&render(&mut s, 120, 40, 0), 80);
    assert_palette_law(&mut s, 120, 40);
    assert_eq!(text[1], "p1 0.1.0   ~/dev/phaseone   main");
    assert_eq!(text[3], "  no journal in this directory.");
    assert_eq!(text[5], "  /resume     reopen a previous session");
    let layout = p1_tui::geometry::layout(120, 40, s.pane_width, false, 2);
    assert!(
        text[layout.composer.y as usize + 1].starts_with("  ⏎ send   ⌥⏎ newline"),
        "{}",
        text[layout.composer.y as usize + 1]
    );
}

#[test]
fn fold_block_screen() {
    let mut s = Screen::new(true);
    s.apply(&tool("shell", "c1", "cargo test -p p1-context boundary"), 0);
    let big: String = (0..94).map(|n| format!("test line {n}\n")).collect();
    s.apply(
        &AgentEvent::ToolFinished {
            result: ToolResultItem {
                call_id: "c1".into(),
                name: "shell".into(),
                status: ToolStatus::Error,
                content: big,
            },
        },
        11_400,
    );
    let full = render(&mut s, 120, 40, 12_000);
    assert_palette_law(&mut s, 120, 40);
    // The failure promoted a PEEK banner over the ledger (SPEC §5).
    assert!(full[1].contains("shell failed"));
    let text = left(&full, 80);
    assert!(text[1].starts_with("✗ shell"));
    assert_eq!(text[2], "  test line 0");
    // The fold handle is stable, addressable, FAINT metadata.
    assert!(text[10].contains("more lines folded → [h-"));
}

#[test]
fn diff_review_blocks_full_width() {
    let mut s = Screen::new(true);
    s.approval = Some(Approval::Diff(DiffView {
        tool: "edit".into(),
        file: "p1-context/src/edge.rs".into(),
        summary: "replace exact string · once".into(),
        position: (1, 3),
        rows: vec![
            DiffRow::Context {
                line: 410,
                text: "let ready = worker.take_summary();".into(),
            },
            DiffRow::Del {
                line: 412,
                text: "    block_until_ready(&worker);".into(),
            },
            DiffRow::Add {
                line: 412,
                text: "    if let Some(summary) = ready {".into(),
            },
        ],
        grantable: true,
    }));
    let text = render(&mut s, 120, 40, 0);
    assert_palette_law(&mut s, 120, 40);
    assert!(text[0].starts_with("! edit      p1-context/src/edge.rs"));
    assert!(text[0].ends_with("1 of 3 files"));
    assert!(text[3].starts_with(" 410"));
    assert!(text[4].contains('−'));
    assert!(text[5].contains('+'));
    assert!(text[38].contains("y  allow once"));
    assert!(text[39].contains("^D next file   ^A all files"));
}

#[test]
fn permission_prompt_with_destructive_floor() {
    let mut s = Screen::new(true);
    s.approval = Some(Approval::Permission(PermissionView {
        command: "rm -rf target/".into(),
        rows: vec![
            ("cwd".into(), "~/dev/phaseone".into()),
            ("sandbox".into(), "bubblewrap · writes: workspace".into()),
            ("network".into(), "off".into()),
        ],
        grantable: false,
    }));
    let text = render(&mut s, 120, 40, 0);
    assert_palette_law(&mut s, 120, 40);
    assert_eq!(text[0], "  rm -rf target/");
    // Decision keys pinned at the bottom; grant rows greyed above them.
    assert!(text[37].contains("y  allow once"));
    assert!(text[38].contains("not grantable — destructive floor"));
    assert!(text[39].contains("not grantable — destructive floor"));
}

#[test]
fn picker_and_status_overlays_dock_above_the_composer() {
    let mut s = Screen::new(true);
    s.picker = Some(Picker {
        groups: vec![PickerGroup {
            header: "ANTHROPIC ROUTE".into(),
            rows: vec![
                PickerRow {
                    label: "claude · sonnet-4.5".into(),
                    value: "300k · $3/$15".into(),
                    available: true,
                    ..PickerRow::default()
                },
                PickerRow {
                    label: "claude · opus-4.8".into(),
                    value: "300k · $15/$75".into(),
                    available: true,
                    ..PickerRow::default()
                },
                PickerRow {
                    label: "glm · 5.3".into(),
                    value: "quota exhausted".into(),
                    available: false,
                    ..PickerRow::default()
                },
            ],
            ..PickerGroup::default()
        }],
        ..Picker::default()
    });
    let text = left(&render(&mut s, 120, 40, 0), 80);
    assert_palette_law(&mut s, 120, 40);
    let header = text
        .iter()
        .position(|row| row.trim() == "ANTHROPIC ROUTE")
        .unwrap();
    let available = text
        .iter()
        .position(|row| row.contains("claude · sonnet-4.5"))
        .unwrap();
    let unavailable = text
        .iter()
        .position(|row| row.contains("quota exhausted"))
        .unwrap();
    assert!(
        header < available && available < unavailable,
        "picker order"
    );
}

#[test]
fn a_failure_states_what_broke_without_a_banner() {
    let mut s = Screen::new(true);
    s.apply(
        &AgentEvent::TurnFinished {
            end: p1_contracts::TurnEnd::ProviderFailed {
                error: p1_contracts::ProviderError::new(
                    p1_contracts::ProviderErrorKind::Transport,
                    "connection dropped",
                ),
            },
        },
        0,
    );
    let text = left(&render(&mut s, 120, 40, 0), 80);
    assert!(
        text.iter()
            .any(|row| row == "provider failed: Transport: connection dropped")
    );
}

#[test]
fn every_state_reads_with_colour_stripped() {
    // Glyphs remain the state carrier without relying on terminal colors.
    let mut s = streaming_screen();
    let text = left(&render(&mut s, 120, 40, 0), 80);
    assert!(text.iter().any(|l| l.contains('›')), "operator turn");
    assert!(text.iter().any(|l| l.contains('✓')), "settled call");
    assert!(text.iter().any(|l| l.contains('▸')), "running call");
    assert!(text.iter().any(|l| l.contains("▪▪▪")), "working indicator");
    assert!(text.iter().any(|l| l.contains('·')), "folded reasoning");
}

#[test]
fn every_screen_renders_at_the_80x24_floor_without_overflow() {
    // The floor law (§6): same glyphs, same shapes, nothing overflows — and
    // nothing panics when the composer or an approval nearly fills the screen.
    for mut s in [
        streaming_screen(),
        {
            let mut s = Screen::new(true);
            s.approval = Some(Approval::Diff(p1_tui::render::diff::DiffView {
                tool: "edit".into(),
                file: "src/x.rs".into(),
                summary: "replace exact string · once".into(),
                position: (1, 1),
                rows: (0..40)
                    .map(|n| p1_tui::render::diff::DiffRow::Add {
                        line: n,
                        text: format!("line {n}"),
                    })
                    .collect(),
                grantable: true,
            }));
            s
        },
        {
            let mut s = Screen::new(true);
            s.approval = Some(Approval::Permission(
                p1_tui::render::permission::PermissionView {
                    command: "rm -rf target/".into(),
                    rows: vec![("cwd".into(), "~/dev".into())],
                    grantable: false,
                },
            ));
            s
        },
    ] {
        let text = render(&mut s, 80, 24, 0);
        assert_palette_law(&mut s, 80, 24);
        assert_eq!(text.len(), 24);
        // A diff review's decision keys are ALWAYS on screen at the floor.
        if s.approval.is_some() {
            let tail = text[20..].join("\n");
            assert!(tail.contains("allow once"), "decision keys visible: {tail}");
        }
    }
}

#[test]
fn output_pane_opens_the_fold_handle() {
    let mut s = Screen::new(true);
    let content: String = (0..60).map(|n| format!("output line {n}\n")).collect();
    let id = p1_tui::fold::FoldId::of(&content);
    s.open_output(p1_tui::render::output::OutputView {
        id: id.clone(),
        lines: content.lines().map(str::to_string).collect(),
        scroll: 0,
    });
    let text = render(&mut s, 120, 40, 0);
    assert_palette_law(&mut s, 120, 40);
    // The pane begins on the geometry top row, below the terminal's blank row.
    assert!(text[1].contains(&format!("OUTPUT [{}]", id)));
    assert!(text[2].contains("output line 0"));
    // Up/Down scroll the pane without moving its header.
    s.scroll_output_by(10);
    let text = render(&mut s, 120, 40, 0);
    assert!(text[2].contains("output line 10"));
}

#[test]
fn pane_width_cycling_changes_the_layout() {
    let mut s = Screen::new(true);
    s.goal = Some("fix compaction boundary stall".into());
    let wide = render(&mut s, 120, 40, 0);
    // The narrow pane is 38 columns; each enabled width retains the goal.
    assert_eq!(
        p1_tui::geometry::layout(120, 40, s.pane_width, false, 2)
            .pane
            .width,
        38
    );
    assert!(wide[1].contains("GOAL"));
    s.cycle_width(); // wide: 56
    let wider = render(&mut s, 120, 40, 0);
    assert_eq!(
        p1_tui::geometry::layout(120, 40, s.pane_width, false, 2)
            .pane
            .width,
        56
    );
    assert!(wider[1].contains("GOAL"));
    s.cycle_width(); // split
    assert_eq!(
        p1_tui::geometry::layout(120, 40, s.pane_width, false, 2)
            .pane
            .width,
        57
    );
    s.cycle_width(); // off
    let off = render(&mut s, 120, 40, 0);
    assert!(!off[1].contains("GOAL"));
    assert_eq!(s.pane_width, PaneWidth::Off);
}
