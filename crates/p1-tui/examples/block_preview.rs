//! Reproducible cell previews and warm-frame timings; no provider or credentials.
#[path = "support/block.rs"]
mod fixture;
use p1_tui::render::screen::draw;
use ratatui::{buffer::Buffer, layout::Rect, style::Color};
fn color(c: Color, bg: bool) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => if bg { "#0a0a0a" } else { "#e8e8e8" }.into(),
    }
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let name = args.get(1).map(String::as_str).unwrap_or("session");
    let w = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(120);
    let h = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(40);
    let mut screen = fixture::screen(name);
    if std::env::var_os("NO_COLOR").is_some() {
        screen.color_mode = p1_tui::palette::ColorMode::Plain;
    }
    if name == "bench" {
        screen.transcript = p1_tui::transcript::Transcript::new();
        let count = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(10_000);
        for i in 0..count {
            screen.transcript.operator(format!(
                "Prompt {i}: inspect the renderer and run its tests."
            ));
        }
        let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
        draw(&mut screen, buf.area, &mut buf, 0);
        let mut timings = vec![];
        for _ in 0..500 {
            let t = std::time::Instant::now();
            draw(&mut screen, buf.area, &mut buf, 0);
            timings.push(t.elapsed().as_micros());
        }
        timings.sort();
        println!(
            "{count} events: warm frame p50={}us p99={}us max={}us (debug build, 500 frames)",
            timings[250], timings[494], timings[499]
        );
        return;
    }
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    draw(&mut screen, buf.area, &mut buf, 0);
    if args.iter().any(|s| s == "--text") {
        for y in 0..h {
            println!(
                "{}",
                (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>()
            );
        }
        return;
    }
    println!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\"><rect width=\"100%\" height=\"100%\" fill=\"#0a0a0a\"/><g font-family=\"DejaVu Sans Mono,monospace\" font-size=\"16\">",
        w as u32 * 10,
        h as u32 * 22
    );
    for y in 0..h {
        for x in 0..w {
            let c = &buf[(x, y)];
            println!(
                "<rect x=\"{}\" y=\"{}\" width=\"10\" height=\"22\" fill=\"{}\"/>",
                x * 10,
                y * 22,
                color(c.bg, true)
            );
        }
    }
    for y in 0..h {
        for x in 0..w {
            let c = &buf[(x, y)];
            let s = c
                .symbol()
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            if s != " " {
                println!(
                    "<text x=\"{}\" y=\"{}\" fill=\"{}\">{s}</text>",
                    x * 10,
                    y * 22 + 17,
                    color(c.fg, false)
                );
            }
        }
    }
    println!("</g></svg>");
}
