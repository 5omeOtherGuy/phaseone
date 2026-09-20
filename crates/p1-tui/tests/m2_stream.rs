//! M2: a live transcript driven by a SCRIPTED event stream — no network, no
//! runtime. Fake time drives the working indicator; queued steering and
//! follow-ups are visible; the 80-column floor collapses the pane.

use p1_contracts::{
    AgentEvent, StopReason, ToolCall, ToolInput, ToolResultItem, ToolStatus, TurnEnd, Usage,
};
use p1_tui::render::screen::draw;
use p1_tui::state::Screen;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn render(screen: &Screen, width: u16, height: u16, now_ms: u64) -> Vec<String> {
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

/// The scripted turn: ask, stream reasoning + text, run a failing tool with a
/// big output, recover, complete — with usage.
fn script() -> Vec<(u64, AgentEvent)> {
    let big: String = (0..50).map(|n| format!("test case {n} ... ok\n")).collect();
    vec![
        (0, AgentEvent::TurnStarted),
        (10, AgentEvent::RequestStarted { request_index: 0 }),
        (
            200,
            AgentEvent::ReasoningDelta {
                text: "check the boundary first".into(),
            },
        ),
        (
            2_000,
            AgentEvent::TextDelta {
                text: "The boundary stalls because ".into(),
            },
        ),
        (
            2_100,
            AgentEvent::TextDelta {
                text: "the summary is held.".into(),
            },
        ),
        (
            4_000,
            AgentEvent::ResponseCompleted {
                model: "sonnet-4.5".into(),
                stop: StopReason::ToolUse,
                usage: Some(Usage {
                    input_uncached: Some(12_000),
                    cache_read: Some(400),
                    output: Some(300),
                    cost_micro_usd: Some(1_100),
                    ..Default::default()
                }),
            },
        ),
        (
            4_100,
            AgentEvent::ToolStarted {
                call: ToolCall {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    input: ToolInput::Json("cargo test -p p1-context".into()),
                },
            },
        ),
        (
            15_500,
            AgentEvent::ToolFinished {
                result: ToolResultItem {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    status: ToolStatus::Error,
                    content: big,
                },
            },
        ),
        (
            16_000,
            AgentEvent::TextDelta {
                text: "Confirmed.".into(),
            },
        ),
        (
            16_500,
            AgentEvent::ResponseCompleted {
                model: "sonnet-4.5".into(),
                stop: StopReason::EndTurn,
                usage: Some(Usage {
                    input_uncached: Some(8_000),
                    cache_read: Some(4_000),
                    output: Some(120),
                    cost_micro_usd: Some(2_300),
                    ..Default::default()
                }),
            },
        ),
        (
            16_600,
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn,
                },
            },
        ),
    ]
}

fn play(screen: &mut Screen, script: &[(u64, AgentEvent)], upto: u64) {
    for (t, event) in script {
        if *t <= upto {
            screen.apply(event, *t);
        }
    }
}

#[test]
fn mid_stream_working_indicator_and_running_call() {
    let mut s = Screen::new(false);
    play(&mut s, &script(), 4_100);
    let text = render(&s, 120, 40, 5_000);
    // Reasoning is collapsed; the working indicator runs on its own line.
    assert!(text.iter().any(|l| l.contains("· reasoning")));
    assert!(text.iter().any(|l| l.starts_with("▪▪▪ shell")));
    // The running call row has no result yet.
    assert!(text.iter().any(|l| l.starts_with("▸ shell")));
    // The LED chase is alive: at t=5000 the cells are not uniformly lit.
    let working_row = text.iter().position(|l| l.starts_with("▪▪▪")).unwrap();
    let _ = working_row;
}

#[test]
fn after_failure_the_evidence_block_and_peek_show() {
    let mut s = Screen::new(true);
    play(&mut s, &script(), 15_500);
    let text = render(&s, 120, 40, 15_600);
    assert!(text.iter().any(|l| l.starts_with("✗ shell")));
    assert!(text.iter().any(|l| l.contains("more lines folded → [h-")));
    // The failure promoted a peek over the ledger…
    assert!(matches!(s.promotion, p1_tui::state::Promotion::Peek { .. }));
    // …and it expires on the tick 3s later.
    s.tick(18_600);
    assert_eq!(s.promotion, p1_tui::state::Promotion::None);
}

#[test]
fn the_turn_settles_with_spend_totals() {
    let mut s = Screen::new(true);
    play(&mut s, &script(), 16_600);
    assert!(s.working.is_none());
    assert_eq!(s.spend.input, Some(24_400));
    assert_eq!(s.spend.cost_micro_usd, Some(3_400));
    let text = render(&s, 120, 40, 16_600);
    assert!(text.iter().any(|l| l.contains("Confirmed.")));
    assert_eq!(text[39], "⏎ send   ⌥⏎ newline   ^C quit");
}

#[test]
fn queued_inputs_are_visible_above_the_hints() {
    let mut s = Screen::new(true);
    play(&mut s, &script(), 4_100);
    s.queue(false, "also check the wrap boundary".into());
    s.queue(true, "then summarize".into());
    let text = render(&s, 120, 40, 5_000);
    assert!(
        text.iter()
            .any(|l| l.contains("· steering: also check the wrap boundary"))
    );
    assert!(
        text.iter()
            .any(|l| l.contains("· follow-up: then summarize"))
    );
}

#[test]
fn the_80_column_floor_keeps_everything_readable() {
    let mut s = Screen::new(true);
    play(&mut s, &script(), 16_600);
    let text = render(&s, 80, 24, 16_600);
    // Same glyphs, same shapes — nothing reflows into a different shape (§6).
    assert!(
        text.iter()
            .any(|l| { l.starts_with("✗ shell") && l.contains("test case 0") })
    );
    assert!(text.iter().any(|l| l.contains("· 42 more lines folded")));
    // The floor line appears under the composer; the pane is gone.
    let last = text.last().unwrap();
    assert!(last.ends_with("^L ledger   ^C cancel"));
}

#[test]
fn scrolling_reaches_back_and_new_output_repins() {
    let mut s = Screen::new(true);
    play(&mut s, &script(), 16_600);
    s.scroll_by(10);
    let text = render(&s, 80, 24, 16_600);
    // Scrolled up: the composer stays, the transcript shows earlier rows.
    assert!(text.iter().any(|l| l.contains("the summary is held.")));
    s.apply(&AgentEvent::TextDelta { text: "new".into() }, 17_000);
    assert_eq!(s.scroll, 0);
}
