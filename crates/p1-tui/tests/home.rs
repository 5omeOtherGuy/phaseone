use p1_tui::{render::screen::draw, state::Screen, transcript::Block};
use ratatui::{buffer::Buffer, layout::Rect};

fn render(screen: &mut Screen, width: u16, height: u16, time: u64) -> Buffer {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    draw(screen, area, &mut buffer, time);
    buffer
}
fn text(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn has_dots(buffer: &Buffer) -> bool {
    buffer
        .content
        .iter()
        .any(|cell| cell.symbol().chars().any(|ch| ch == '●'))
}
#[test]
fn welcome_is_static_without_changing_the_composer() {
    let mut screen = Screen::new(false);
    let first = render(&mut screen, 120, 40, 0);
    let middle = render(&mut screen, 120, 40, 12_000);
    assert!(has_dots(&first));
    assert_eq!(first, middle);
    assert_eq!(first, render(&mut screen, 120, 40, 24_000));
    assert_eq!(&first.content[38 * 120..], &middle.content[38 * 120..]);
    assert!(text(&first).contains("phaseone"));
    assert!(text(&first).contains("We love pie"));
}
#[test]
fn reduced_motion_freezes_the_whole_scene() {
    let mut screen = Screen::new(true);
    assert_eq!(
        render(&mut screen, 100, 35, 0),
        render(&mut screen, 100, 35, 14_000)
    );
}
#[test]
fn welcome_does_not_replace_history_or_focus_mode() {
    let mut screen = Screen::new(false);
    screen.transcript.blocks.push(Block::Info {
        lines: vec!["welcome metadata".into()],
    });
    let greeting = render(&mut screen, 80, 24, 0);
    assert!(text(&greeting).starts_with("welcome metadata"));
    assert!(has_dots(&greeting));
    screen.transcript.operator("start work");
    assert!(!has_dots(&render(&mut screen, 80, 24, 0)));
    let mut focused = Screen::new(false);
    focused.focus = true;
    assert!(!has_dots(&render(&mut focused, 120, 40, 0)));
}
#[test]
fn small_or_offset_viewports_never_overwrite_surrounding_cells() {
    for (width, height) in [(0, 0), (1, 1), (16, 5), (30, 10), (80, 24)] {
        let area = Rect::new(3, 2, width, height);
        let mut buf = Buffer::empty(Rect::new(0, 0, 90, 40));
        let mut s = Screen::new(false);
        draw(&mut s, area, &mut buf, 6_000);
        assert_eq!(buf[(0, 0)].symbol(), " ");
        assert_eq!(buf[(89, 39)].symbol(), " ");
    }
}

#[test]
fn working_and_status_overlays_suppress_the_home() {
    let mut screen = Screen::new(false);
    screen.status = Some(Vec::new());
    assert!(!has_dots(&render(&mut screen, 120, 40, 0)));
    screen.status = None;
    screen.working = Some(p1_tui::state::Working {
        label: "working".into(),
        started_ms: 0,
    });
    assert!(!has_dots(&render(&mut screen, 120, 40, 0)));
}
