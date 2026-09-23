//! Tail-only transcript render at the screen level: a huge transcript must
//! draw the same last screenful as a full build, without building every row.

use p1_tui::render::screen::draw;
use p1_tui::state::Screen;
use p1_tui::transcript::Block;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn render(screen: &mut Screen, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw(screen, frame.area(), frame.buffer_mut(), 0))
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

#[test]
fn a_5000_block_screen_draws_only_the_last_screenful() {
    let mut s = Screen::new(true);
    for n in 0..5_000 {
        s.transcript.blocks.push(Block::Prose {
            lines: vec![format!("prose line {n}")],
        });
    }
    let text = render(&mut s, 120, 40);
    // The transcript pins to the bottom: the visible prose rows are a
    // consecutive run ending at the newest block (the composer takes a couple
    // of rows, so fewer than 40 fit).
    // Transcript text starts at the §4 inset (column 2), not column 0.
    let nums: Vec<usize> = text
        .iter()
        .map(|line| line.trim_start())
        .filter(|line| line.starts_with("prose line"))
        .map(|line| line["prose line ".len()..].parse().unwrap())
        .collect();
    assert!(!nums.is_empty());
    assert!(nums.len() <= 40, "never draws more rows than the area");
    assert_eq!(*nums.last().unwrap(), 4_999, "the newest row is visible");
    assert!(
        nums.windows(2).all(|w| w[1] == w[0] + 1),
        "the visible rows are consecutive: {nums:?}"
    );
    // The scroll math still sees the whole transcript, not the rendered tail.
    assert_eq!(s.last_rendered.0, 9_999);
}
