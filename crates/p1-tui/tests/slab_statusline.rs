mod common;
use common::slab;
use p1_tui::render::statusbar::StatusBar;
use ratatui::{buffer::Buffer, layout::Rect};

#[test]
fn statusline_matches_all_reference_widths() {
    let status = StatusBar {
        model: Some("claude/opus-5.5".into()),
        effort: Some("high".into()),
        repo: Some("phaseone".into()),
        branch: Some("main".into()),
        workers: 2,
        ctx: Some("10%".into()),
        ctx_warn: false,
        spend: None,
        clock: Some("0h14".into()),
        diff: None,
    };
    for (w, id) in [
        (116, "el-statusline@116"),
        (96, "el-statusline@96"),
        (76, "el-statusline@76"),
        (52, "el-statusline@52"),
    ] {
        let mut buf = Buffer::empty(Rect::new(0, 0, w, 1));
        buf.set_line(0, 0, &status.line(w as usize), w);
        slab::assert_mock(&buf, buf.area, id);
    }
}
