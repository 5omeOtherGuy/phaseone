#[path = "../examples/support/block.rs"]
mod fixture;
use p1_tui::{
    palette as p,
    render::{block, screen::draw},
    state::{EditAction, Screen},
    transcript::Block,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Color, text::Line};
fn plain(lines: &[Line<'_>]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn render(s: &mut Screen, w: u16, h: u16) -> Buffer {
    let mut b = Buffer::empty(Rect::new(0, 0, w, h));
    draw(s, b.area, &mut b, 0);
    b
}
const TOOLS: &[&str] = &[
    "read", "shell", "write", "edit", "skill", "search", "send", "stop", "delegate", "ask",
    "notify", "compact",
];
#[test]
fn tool_goldens_and_every_width_80_through_200() {
    for name in TOOLS {
        let s = fixture::screen(name);
        let expected = std::fs::read_to_string(format!(
            "{}/tests/golden/{name}.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("golden fixture");
        let lines = block::lines(&s.transcript, 76, None, 0, true);
        assert_eq!(plain(&lines) + "\n", expected, "{name}");
        for width in 80..=200 {
            let lines = block::lines(&s.transcript, width, None, 0, true);
            assert_eq!(
                lines.len(),
                block::lines(&s.transcript, 200, None, 0, true).len(),
                "body never wraps: {name} width {width}"
            );
            for line in &lines {
                assert_eq!(line.width(), width, "{name} {width}");
                assert!(
                    line.spans.iter().all(|s| s.style.bg.is_some()),
                    "every trailing cell painted"
                );
                assert!(
                    !plain(std::slice::from_ref(line))
                        .chars()
                        .any(|c| ('\u{2500}'..='\u{257f}').contains(&c))
                );
            }
            assert_eq!(
                lines
                    .iter()
                    .filter(|l| l.style.bg == Some(p::BLOCK_PLUS))
                    .count(),
                1,
                "exactly one header"
            );
        }
    }
}
#[test]
fn full_screen_insets_gutter_and_resize_roundtrip() {
    let mut s = fixture::screen("session");
    let before = render(&mut s, 120, 40);
    render(&mut s, 80, 24);
    assert_eq!(before, render(&mut s, 120, 40));
    for w in [80, 100, 120, 200] {
        let b = render(&mut s, w, 40);
        for y in 0..40 {
            for x in [0, 1, w - 2, w - 1] {
                assert_eq!(b[(x, y)].bg, p::GROUND);
            }
        }
        for x in 0..w {
            assert_eq!(b[(x, 0)].symbol(), " ");
            assert_eq!(b[(x, 39)].symbol(), " ");
        }
        if w >= 100 {
            for y in 0..38 {
                for x in [w - 44, w - 43] {
                    assert_eq!(b[(x, y)].bg, p::GROUND);
                }
            }
        }
        assert_eq!(b[(2, 38)].bg, p::BLOCK_PLUS);
    }
}
#[test]
fn large_outputs_bounded_and_addressable() {
    for (name, n) in [("read", 500), ("shell", 4000), ("edit", 1600)] {
        let mut s = Screen::new(true);
        let body = (0..n).map(|i| format!("line {i}\n")).collect::<String>();
        fixture::call(&mut s, name, "a", &body);
        let lines = block::lines(&s.transcript, 76, None, 0, true);
        assert_eq!(lines.len(), 26);
        let id = s.transcript.latest_fold.as_ref().unwrap();
        assert_eq!(id.0.len(), 6);
        assert_eq!(s.transcript.output(id), Some(body.as_str()));
        let text = plain(&lines);
        assert!(text.contains(&format!("[{id}]")));
        if name == "shell" {
            assert!(text.contains("line 3999"));
            assert!(!text.contains("line 0 "));
        }
    }
    let mut s = Screen::default();
    fixture::call(&mut s, "read", "a", &"line\n".repeat(40));
    assert_eq!(block::lines(&s.transcript, 80, None, 0, true).len(), 41);
}
#[test]
fn editor_multiline_paste_history_and_cursor_stay_visible() {
    let mut s = Screen::default();
    s.composer
        .insert_text("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n你好");
    let b = render(&mut s, 80, 24);
    let (x, y) = s.cursor_position.unwrap();
    assert!(x < 78 && y < 21);
    assert_eq!(b[(x, y)].bg, p::BLOCK_PLUS);
    s.edit(EditAction::Home);
    assert_eq!(s.composer.cursor, s.composer.text.chars().count() - 2);
    s.edit(EditAction::Delete);
    assert!(s.composer.text.ends_with('好'));
    s.remember_input("first prompt");
    s.composer.take();
    s.composer.insert_text("draft");
    s.edit(EditAction::Up);
    assert_eq!(s.composer.text, "first prompt");
    s.edit(EditAction::Down);
    assert_eq!(s.composer.text, "draft");
}
#[test]
fn no_color_preserves_every_glyph_and_literal_diff_sign() {
    for name in TOOLS {
        let mut s = fixture::screen(name);
        s.pane_width = p1_tui::state::PaneWidth::Off;
        let color = render(&mut s, 80, 40);
        s.color_mode = p::ColorMode::Plain;
        let plain = render(&mut s, 80, 40);
        for (a, b) in color.content.iter().zip(&plain.content) {
            assert_eq!(a.symbol(), b.symbol());
            assert_eq!(b.fg, Color::Reset);
            assert_eq!(b.bg, Color::Reset);
        }
    }
}
#[test]
fn settled_layout_is_cached_and_viewport_matches_full_render() {
    let mut s = Screen::new(true);
    for i in 0..1000 {
        s.transcript.operator(format!("message {i}"));
    }
    let full = block::lines(&s.transcript, 76, None, 0, true);
    for top in [None, Some(0), Some(913)] {
        let (total, visible) = block::viewport(&s.transcript, 76, 30, top, None, 0, true);
        assert_eq!(total, full.len());
        let start = top.unwrap_or(total - 30);
        assert_eq!(visible, full[start..start + 30]);
    }
    // Settling a call invalidates its position, even with prose after it.
    fixture::call(&mut s, "shell", "test", "done\n[exit code: 0]");
    let (_, visible) = block::viewport(&s.transcript, 76, 30, None, None, 0, true);
    assert!(plain(&visible).contains("done"));
}
#[test]
fn control_sequences_wide_cells_and_long_commands_are_safe() {
    assert_eq!(block::clean("a\t\x1b[31mred\x1b[0m\x07"), "a       red");
    for width in [80, 100, 120, 200] {
        let line = block::header(
            "shell",
            &"界".repeat(300),
            "✗ 11.4s · exit 101",
            width,
            false,
        );
        assert_eq!(line.width(), width);
        assert!(plain(&[line]).trim_end().ends_with("exit 101"));
        let line = block::body(&"界".repeat(300), width, p::DIM, p::BLOCK);
        assert_eq!(line.width(), width);
        assert!(plain(&[line]).trim_end().ends_with('›'));
    }
    assert_eq!(p1_tui::wrap::wrap("界界", 1), vec!["›", "›", ""]);
}
#[test]
fn event_spacing_reduced_motion_and_required_text_contrast() {
    let mut s = fixture::screen("session");
    let a = render(&mut s, 120, 40);
    let mut b = Buffer::empty(a.area);
    draw(&mut s, a.area, &mut b, 999);
    assert_eq!(a, b);
    let lines = block::lines(&s.transcript, 76, None, 0, true);
    let blank = lines.iter().filter(|l| l.spans.is_empty()).count();
    assert_eq!(blank, s.transcript.blocks.len() - 1);
    for block in &s.transcript.blocks {
        if let Block::Call(row) = block {
            for line in block::call_lines(row, 76, 0, true) {
                for span in line.spans {
                    if span.content.contains("exit") || span.content.contains("src/renderer.rs") {
                        assert_ne!(span.style.fg, Some(p::FAINT));
                    }
                }
            }
        }
    }
}

#[test]
fn output_filter_and_horizontal_pan_do_not_wrap_or_render_offscreen_rows() {
    let view = p1_tui::render::output::OutputView {
        id: p1_tui::fold::FoldId("h-1234".into()),
        lines: (0..4000)
            .map(|n| format!("{n:04} long content matching row"))
            .collect(),
        scroll: 0,
    };
    let filtered = p1_tui::render::output::view_lines(&view, 20, "3999", 0, 8);
    let text = plain(&filtered);
    assert!(text.contains("3999"));
    assert!(!text.contains("3998"));
    assert!(filtered.len() <= 8);
    let panned = p1_tui::render::output::view_lines(&view, 20, "", 5, 8);
    assert!(plain(&panned).contains("long content"));
    assert!(panned.iter().all(|l| l.width() == 20));
}

#[test]
fn short_handles_survive_replay_and_host_notes_do_not_change_them() {
    use p1_contracts::{AgentEvent, ToolCall, ToolInput, ToolResultItem, ToolStatus};
    let start = AgentEvent::ToolStarted {
        call: ToolCall {
            call_id: "journal-call-1".into(),
            name: "shell".into(),
            input: ToolInput::Json("{}".into()),
        },
    };
    let end = AgentEvent::ToolFinished {
        result: ToolResultItem {
            call_id: "journal-call-1".into(),
            name: "shell".into(),
            status: ToolStatus::Ok,
            content: "hello".into(),
        },
    };
    let mut a = Screen::default();
    a.transcript.note("extra host notice");
    a.transcript.apply(&start, None);
    a.transcript.apply(&end, None);
    let mut b = Screen::default();
    b.transcript.apply(&start, None);
    b.transcript.apply(&end, None);
    assert_eq!(a.transcript.latest_fold, b.transcript.latest_fold);
}

#[test]
fn indexed_diff_colors_remain_distinct_and_no_box_glyphs_in_any_renderer_source() {
    let mut s = fixture::screen("edit");
    s.color_mode = p::ColorMode::Indexed;
    let b = render(&mut s, 80, 24);
    assert!(b.content.iter().any(|c| c.bg == Color::Indexed(22)));
    assert!(b.content.iter().any(|c| c.bg == Color::Indexed(52)));
    for entry in std::fs::read_dir(format!("{}/src/render", env!("CARGO_MANIFEST_DIR"))).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") {
            let src = std::fs::read_to_string(path).unwrap();
            assert!(!src.chars().any(|c| ('\u{2500}'..='\u{257f}').contains(&c)));
        }
    }
}

#[test]
fn narrow_output_overlay_clears_underlying_transcript_and_reused_buffer() {
    let mut s = fixture::screen("session");
    s.transcript
        .operator("UNDERLYING TRANSCRIPT MUST DISAPPEAR");
    s.output_focus = true;
    s.pane_mode = p1_tui::state::PaneMode::Output;
    s.output = Some(p1_tui::render::output::OutputView {
        id: p1_tui::fold::FoldId("h-1234".into()),
        lines: vec!["unique filtered result".into()],
        scroll: 0,
    });
    let mut b = Buffer::filled(Rect::new(0, 0, 80, 24), ratatui::buffer::Cell::new("Z"));
    draw(&mut s, b.area, &mut b, 0);
    let text = b.content.iter().map(|c| c.symbol()).collect::<String>();
    assert!(text.contains("unique filtered result"));
    assert!(!text.contains("UNDERLYING"));
    assert!(!text.contains('Z'));
    assert_eq!(b[(3, 3)].symbol(), " ");
}

#[test]
fn mouse_disclosure_and_wheel_follow_the_rendered_viewport() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut s = fixture::screen("shell");
    if let Block::Call(row) = &mut s.transcript.blocks[0] {
        row.output = Some((0..100).map(|n| format!("mouse row {n}\n")).collect());
    }
    s.scroll_top = Some(0);
    render(&mut s, 120, 40);
    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let (rect, hit) = s.tool_hits[0].clone();
    let block::Hit::Header(index) = hit else {
        panic!("the first hit is the tool header");
    };
    s.on_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        rect.x + 2,
        rect.y,
    ));
    render(&mut s, 120, 40);
    assert_eq!(s.transcript.disclosures.get(&index), Some(&true));
    let expanded = s.last_rendered.0;
    s.on_mouse(mouse(MouseEventKind::ScrollDown, rect.x, rect.y));
    render(&mut s, 120, 40);
    assert!(s.scroll_top.is_some_and(|top| top > 0));
    s.scroll_top = Some(0);
    render(&mut s, 120, 40);
    s.on_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        rect.x + 2,
        rect.y,
    ));
    render(&mut s, 120, 40);
    assert!(s.last_rendered.0 < expanded);
    // A folded block returns to its preview (handle and all), not to a bare header.
    assert_eq!(s.transcript.disclosures.get(&index), None);
    assert_eq!(s.scroll_top, None);
}

#[test]
fn wheel_routes_by_pointer_and_modal_layers_block_hidden_headers() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use p1_tui::render::output::OutputView;
    let mut s = fixture::screen("session");
    s.open_output(OutputView {
        id: p1_tui::fold::FoldId::of("mouse"),
        lines: (0..100).map(|n| format!("output {n}")).collect(),
        scroll: 0,
    });
    render(&mut s, 120, 40);
    let wheel = |x, y| MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    s.on_mouse(wheel(s.output_area.x, s.output_area.y));
    assert_eq!(s.output.as_ref().unwrap().scroll, 3);
    let old = s.output.as_ref().unwrap().scroll;
    s.on_mouse(wheel(s.transcript_area.x, s.transcript_area.y));
    assert_eq!(s.output.as_ref().unwrap().scroll, old);
    render(&mut s, 80, 24);
    assert!(
        s.tool_hits.is_empty(),
        "overlay hides transcript hit targets"
    );
    s.on_mouse(wheel(s.output_area.x, s.output_area.y));
    assert_eq!(s.output.as_ref().unwrap().scroll, old + 3);
    s.ledger_overlay = true;
    s.on_mouse(wheel(s.output_area.x, s.output_area.y));
    assert_eq!(s.output.as_ref().unwrap().scroll, old + 3);
    s.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        ..wheel(4, 1)
    });
    assert!(s.transcript.disclosures.is_empty());
    s.ledger_overlay = false;
    s.scroll_output_by(10000);
    render(&mut s, 120, 40);
    let visible = s.output_area.height as usize - 2;
    assert_eq!(
        s.output.as_ref().unwrap().scroll,
        100 - visible,
        "bottom keeps a full page visible"
    );
    s.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        ..wheel(s.composer_area.x, s.composer_area.y)
    });
    assert!(!s.output_focus, "clicking the composer restores typing");
}

#[test]
fn scrolling_detaches_and_returns_to_live_without_losing_a_draft() {
    use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
    let mut s = fixture::screen("session");
    s.transcript.blocks.push(Block::Prose {
        lines: (0..200).map(|n| format!("history {n}")).collect(),
    });
    s.composer = Default::default();
    s.composer.insert_text("draft\nsecond line");
    render(&mut s, 100, 30);
    let mut mouse = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 4,
        row: 5,
        modifiers: KeyModifiers::NONE,
    };
    s.on_mouse(mouse);
    let top = s.scroll_top.unwrap();
    s.transcript.blocks.push(Block::Prose {
        lines: vec!["new live output".into()],
    });
    render(&mut s, 100, 30);
    assert_eq!(s.scroll_top, Some(top));
    assert!(s.live_area.width > 0);
    mouse.kind = MouseEventKind::ScrollDown;
    for _ in 0..10 {
        s.on_mouse(mouse);
    }
    assert_eq!(s.scroll_top, None);
    assert_eq!(s.composer.text, "draft\nsecond line");
}
