//! Frame timings over a realistic session: prompts, streamed prose, shell calls with
//! folded output, reads and edits. Measures what an operator feels: the first frame
//! after resuming N turns, a warm frame, a resize, a frame while prose streams, and a
//! scroll step. No provider, no terminal.
//!
//! cargo run -p p1-tui --example frame_bench [--release] -- [turns] [width] [height]
use p1_contracts::{AgentEvent, StopReason, ToolCall, ToolInput, ToolResultItem, ToolStatus};
use p1_tui::render::screen::draw;
use p1_tui::state::Screen;
use ratatui::{buffer::Buffer, layout::Rect};
use std::time::Instant;

fn session(turns: usize) -> Screen {
    let mut screen = Screen::new(true);
    let mut now = 0;
    let mut step = |screen: &mut Screen, event: AgentEvent| {
        now += 7;
        screen.apply(&event, now);
    };
    for turn in 0..turns {
        screen.transcript.operator(format!(
            "Turn {turn}: inspect the renderer and run its tests."
        ));
        step(&mut screen, AgentEvent::TurnStarted);
        for word in
            "Looking at the cache invalidation path first, then the tests.".split_inclusive(' ')
        {
            step(&mut screen, AgentEvent::TextDelta { text: word.into() });
        }
        let tools = [
            (
                "shell",
                r#"{"command":"cargo test -p p1-tui"}"#.to_string(),
                (0..30)
                    .map(|i| format!("test case_{i} ... ok\n"))
                    .collect::<String>()
                    + "[exit code: 0]",
            ),
            (
                "read",
                r#"{"file_path":"crates/p1-tui/src/render/block.rs"}"#.to_string(),
                (1..=60)
                    .map(|i| format!("{i:>6}\tlet value_{i} = compute({i});\n"))
                    .collect(),
            ),
            (
                "edit",
                r#"{"file_path":"src/lib.rs","old_string":"a\nb","new_string":"c\nd\ne"}"#
                    .to_string(),
                "Edited src/lib.rs".to_string(),
            ),
        ];
        for (n, (name, input, output)) in tools.into_iter().enumerate() {
            let id = format!("c{turn}_{n}");
            step(
                &mut screen,
                AgentEvent::ToolStarted {
                    call: ToolCall {
                        call_id: id.clone(),
                        name: name.into(),
                        input: ToolInput::Json(input),
                    },
                },
            );
            step(
                &mut screen,
                AgentEvent::ToolFinished {
                    result: ToolResultItem {
                        call_id: id,
                        name: name.into(),
                        status: ToolStatus::Ok,
                        content: output,
                    },
                },
            );
        }
        step(
            &mut screen,
            AgentEvent::ResponseCompleted {
                model: "m".into(),
                stop: StopReason::EndTurn,
                usage: None,
            },
        );
        step(
            &mut screen,
            AgentEvent::TurnFinished {
                end: p1_contracts::TurnEnd::Completed {
                    stop: StopReason::EndTurn,
                },
            },
        );
    }
    screen
}

fn stats(label: &str, mut t: Vec<u128>) {
    t.sort();
    let n = t.len();
    println!(
        "{label:<28} p50={:>7}us p99={:>7}us max={:>7}us (n={n})",
        t[n / 2],
        t[(n * 99 / 100).min(n - 1)],
        t[n - 1]
    );
}

fn main() {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let turns = args.first().copied().unwrap_or(1000);
    let (w, h) = (
        args.get(1).copied().unwrap_or(120) as u16,
        args.get(2).copied().unwrap_or(40) as u16,
    );
    let built = Instant::now();
    let mut screen = session(turns);
    println!(
        "{turns} turns ({} blocks) built in {}ms",
        screen.transcript.blocks.len(),
        built.elapsed().as_millis()
    );
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    let t = Instant::now();
    draw(&mut screen, buf.area, &mut buf, 0);
    println!("{:<28} {}us", "cold first frame", t.elapsed().as_micros());

    stats(
        "warm frame",
        (0..200)
            .map(|_| {
                let t = Instant::now();
                draw(&mut screen, buf.area, &mut buf, 0);
                t.elapsed().as_micros()
            })
            .collect(),
    );

    let mut resize = vec![];
    for i in 0..20u16 {
        let width = if i % 2 == 0 { w - 1 - i % 7 } else { w };
        let mut b = Buffer::empty(Rect::new(0, 0, width, h));
        let t = Instant::now();
        draw(&mut screen, b.area, &mut b, 0);
        resize.push(t.elapsed().as_micros());
    }
    stats("resize frame", resize);

    let mut scroll = vec![];
    for _ in 0..100 {
        screen.scroll_by(3);
        let t = Instant::now();
        draw(&mut screen, buf.area, &mut buf, 0);
        scroll.push(t.elapsed().as_micros());
    }
    stats("scroll step frame", scroll);
    screen.scroll_top = None;

    screen.apply(&AgentEvent::TurnStarted, 1);
    let mut streaming = vec![];
    for i in 0..400 {
        screen.apply(
            &AgentEvent::TextDelta {
                text: if i % 12 == 11 {
                    "\n".into()
                } else {
                    format!("word{i} ")
                },
            },
            2,
        );
        let t = Instant::now();
        draw(&mut screen, buf.area, &mut buf, i);
        streaming.push(t.elapsed().as_micros());
    }
    stats("streaming delta frame", streaming);
}
