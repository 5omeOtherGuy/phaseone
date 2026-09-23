//! Inspect the actual terminal cells without a provider, credentials or network.
//! cargo run -p p1-tui --example home_preview -- 80 24 6000 > /tmp/home.svg
use p1_tui::{
    band::Seg,
    palette::INK,
    render::{home::HomePrelude, screen::draw},
    state::Screen,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Color};

fn color(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => "#e8e8e8".into(),
    }
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let width = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(120);
    let height = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(40);
    let time = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut screen = Screen::new(false);
    screen.home = Some(HomePrelude {
        version: "0.1.0".into(),
        path: "~/dev/phaseone".into(),
        branch: Some("main".into()),
        state: vec![vec![Seg::new(INK, "no journal in this directory.")]],
        items: [
            ("/resume", "reopen a previous session"),
            ("/model", "choose a model"),
            ("/access", "full · --ask to confirm"),
            ("/goal", "set the session objective"),
        ]
        .iter()
        .map(|(command, what)| (command.to_string(), what.to_string()))
        .collect(),
    });
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    draw(&mut screen, area, &mut buffer, time);
    println!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\"><rect width=\"100%\" height=\"100%\" fill=\"#0a0a0a\"/><g font-family=\"DejaVu Sans Mono, monospace\" font-size=\"16\">",
        u32::from(width) * 10,
        u32::from(height) * 20
    );
    for y in 0..height {
        for x in 0..width {
            let cell = &buffer[(x, y)];
            println!(
                "<rect x=\"{}\" y=\"{}\" width=\"10\" height=\"20\" fill=\"{}\"/>",
                x * 10,
                y * 20,
                color(cell.bg)
            );
            let symbol = cell
                .symbol()
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            if symbol != " " {
                println!(
                    "<text x=\"{}\" y=\"{}\" fill=\"{}\">{symbol}</text>",
                    x * 10,
                    y * 20 + 16,
                    color(cell.fg)
                );
            }
        }
    }
    println!("</g></svg>");
}
