//! Finishing-pass contracts: lazy layout matches the full render, scroll anchors
//! survive width changes, disclosure and follow semantics, prose that never loses
//! text, and rows for every call the model made.
#[path = "../examples/support/block.rs"]
#[allow(dead_code)]
mod fixture;
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use p1_contracts::{AgentEvent, ToolCall, ToolInput, ToolResultItem, ToolStatus, TurnEnd};
use p1_tui::{
    render::{block, screen::draw},
    state::Screen,
    transcript::Block,
};
use ratatui::{buffer::Buffer, layout::Rect, text::Line};

fn plain(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn render(s: &mut Screen, w: u16, h: u16) -> Vec<String> {
    let mut b = Buffer::empty(Rect::new(0, 0, w, h));
    draw(s, b.area, &mut b, 0);
    (0..h)
        .map(|y| (0..w).map(|x| b[(x, y)].symbol()).collect::<String>())
        .collect()
}

fn click(s: &mut Screen, column: u16, row: u16) {
    s.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
}

/// A session with every block shape, including the awkward ones: empty host
/// text, prose with tabs, lists, fences and blank-line runs, reasoning.
fn mixed() -> Screen {
    let mut s = Screen::new(true);
    for turn in 0..12 {
        s.transcript
            .operator(format!("turn {turn}: please look at\tthis   aligned  text"));
        s.transcript.blocks.push(Block::Info { lines: vec![] });
        s.apply(
            &AgentEvent::ReasoningDelta {
                text: "weighing the options\n".into(),
            },
            10,
        );
        s.apply(
            &AgentEvent::TextDelta {
                text: "\n\nIntro with\ttabs\tand words that keep going well past any narrow width.\n\n\n- a list item long enough to wrap onto a second row at sixty columns\n1. numbered item that also wraps when the width is narrow enough\n```\nlet code =    \"never wrapped, even when it is far wider than the transcript column\";\n```\n\n".into(),
            },
            20,
        );
        fixture::call(
            &mut s,
            "shell",
            r#"{"command":"cargo test"}"#,
            &((0..(turn * 7))
                .map(|n| format!("out {n}\n"))
                .collect::<String>()
                + "[exit code: 0]"),
        );
        s.apply(
            &AgentEvent::ResponseCompleted {
                model: "m".into(),
                stop: p1_contracts::StopReason::EndTurn,
                usage: None,
            },
            30,
        );
    }
    s
}

#[test]
fn lazy_viewport_equals_the_full_render_at_every_width_and_offset() {
    let s = mixed();
    for width in [40, 57, 60, 76, 100, 140] {
        let full = block::lines(&s.transcript, width, None, 0, true);
        for height in [7, 23] {
            for top in (0..full.len().saturating_sub(height)).step_by(5) {
                let (total, visible) =
                    block::viewport(&s.transcript, width, height, Some(top), None, 0, true);
                assert_eq!(total, full.len(), "width {width}");
                assert_eq!(
                    visible.iter().map(plain).collect::<Vec<_>>(),
                    full[top..top + height]
                        .iter()
                        .map(plain)
                        .collect::<Vec<_>>(),
                    "width {width} top {top}"
                );
            }
        }
    }
}

#[test]
fn never_two_blank_rows_and_prose_never_loses_a_word() {
    let s = mixed();
    for width in 40..=140 {
        let rows: Vec<String> = block::lines(&s.transcript, width, None, 0, true)
            .iter()
            .map(plain)
            .collect();
        for pair in rows.windows(2) {
            assert!(
                !(pair[0].trim().is_empty() && pair[1].trim().is_empty()),
                "double blank row at width {width}"
            );
        }
        let text = rows.join(" ");
        for word in ["tabs", "and", "going", "past", "narrow", "width."] {
            assert!(text.contains(word), "{word} lost at width {width}");
        }
        // Prose wraps; only fenced code is clipped (with the FAINT cut mark).
        for row in &rows {
            if row.trim_end().ends_with('›') {
                assert!(row.contains("let code"), "prose clipped at {width}: {row}");
            }
        }
    }
    let rows: Vec<String> = block::lines(&s.transcript, 60, None, 0, true)
        .iter()
        .map(plain)
        .collect();
    // Interior spacing survives; list continuations hang under the item text.
    assert!(rows.iter().any(|r| r.contains("this   aligned  text")));
    let item = rows
        .iter()
        .position(|r| r.trim_start().starts_with("- a list item"))
        .unwrap();
    assert!(rows[item + 1].starts_with("    ") && !rows[item + 1].trim().is_empty());
    let lead = |r: &str| r.len() - r.trim_start().len();
    assert_eq!(lead(&rows[item + 1]), lead(&rows[item]) + 2);
}

#[test]
fn a_resize_keeps_the_reader_on_the_same_content_and_relays_out_only_what_shows() {
    let mut s = mixed();
    render(&mut s, 100, 30);
    s.scroll_by(60);
    let before = render(&mut s, 100, 30);
    let anchor = before[1].trim().to_owned();
    assert!(!anchor.is_empty());
    let rendered = s.transcript.rendered_events();
    for (w, h) in [(60, 20), (140, 45), (100, 30)] {
        let rows = render(&mut s, w, h);
        let first = rows[1].trim();
        assert!(
            anchor.starts_with(first) || first.starts_with(anchor.split(' ').next().unwrap()),
            "{w}x{h}: top row {first:?} is not the anchored {anchor:?}"
        );
    }
    assert_eq!(render(&mut s, 100, 30)[1].trim(), anchor);
    // Three width changes re-rendered only the rows on screen, not the history.
    assert!(s.transcript.rendered_events() - rendered < 40 * 3);
}

#[test]
fn a_header_click_at_the_live_tail_keeps_following() {
    let mut s = mixed();
    s.transcript.operator("tail");
    fixture::call(
        &mut s,
        "shell",
        r#"{"command":"cargo test"}"#,
        "a\nb\nc\n[exit code: 0]",
    );
    render(&mut s, 100, 30);
    assert_eq!(s.scroll_top, None);
    let header = s
        .tool_hits
        .iter()
        .rev()
        .find(|(_, hit)| matches!(hit, block::Hit::Header(_)))
        .map(|(rect, _)| *rect)
        .unwrap();
    click(&mut s, header.x + 3, header.y);
    let rows = render(&mut s, 100, 30);
    assert_eq!(s.scroll_top, None, "collapsing at the tail re-follows");
    assert!(!rows.iter().any(|r| r.contains("below")));
    assert!(rows.iter().any(|r| r.contains("· 3 lines hidden")));
    // The second click shows the body again.
    let header = s
        .tool_hits
        .iter()
        .rev()
        .find(|(_, hit)| matches!(hit, block::Hit::Header(_)))
        .map(|(rect, _)| *rect)
        .unwrap();
    click(&mut s, header.x + 3, header.y);
    let rows = render(&mut s, 100, 30);
    assert!(!rows.iter().any(|r| r.contains("hidden")));
    assert!(!rows.iter().any(|r| r.contains("0 lines below")));
}

#[test]
fn a_fold_row_click_opens_that_output_and_a_reasoning_row_toggles() {
    let mut s = mixed();
    render(&mut s, 120, 40);
    s.scroll_top = Some(0);
    // Find an older folded block's fold row by scrolling until one is visible.
    let mut found = None;
    for top in 0..s.last_rendered.0 {
        s.scroll_top = Some(top);
        render(&mut s, 120, 40);
        if let Some((rect, block::Hit::Fold(id))) = s
            .tool_hits
            .iter()
            .find(|(_, h)| matches!(h, block::Hit::Fold(_)))
            .cloned()
        {
            found = Some((rect, id));
            break;
        }
    }
    let (rect, id) = found.expect("a visible fold row");
    click(&mut s, rect.x + 4, rect.y);
    assert_eq!(s.output.as_ref().map(|o| o.id.clone()), Some(id));
    assert!(s.output_focus);

    s.output_focus = false;
    let mut found = None;
    for top in 0..s.last_rendered.0 {
        s.scroll_top = Some(top);
        render(&mut s, 120, 40);
        found = s.tool_hits.iter().find_map(|(r, h)| match h {
            block::Hit::Reasoning(i) => Some((*r, *i)),
            _ => None,
        });
        if found.is_some() {
            break;
        }
    }
    let (rect, index) = found.expect("a visible reasoning row");
    click(&mut s, rect.x + 4, rect.y);
    assert!(matches!(
        s.transcript.blocks[index],
        Block::Reasoning { expanded: true, .. }
    ));
}

fn result(id: &str, name: &str, status: ToolStatus, content: &str) -> AgentEvent {
    AgentEvent::ToolFinished {
        result: ToolResultItem {
            call_id: id.into(),
            name: name.into(),
            status,
            content: content.into(),
        },
    }
}

#[test]
fn denied_and_unstarted_calls_get_their_row_live() {
    let mut s = Screen::new(true);
    s.apply(&AgentEvent::TurnStarted, 0);
    let call = ToolCall {
        call_id: "w1".into(),
        name: "write".into(),
        input: ToolInput::Json(r#"{"file_path":"notes.txt","content":"x\n"}"#.into()),
    };
    s.transcript.announce(&call);
    s.apply(
        &result("w1", "write", ToolStatus::Denied, "Denied by the user."),
        5,
    );
    // Never announced, never started: still a row, named after its result.
    s.apply(
        &result(
            "r9",
            "read",
            ToolStatus::Cancelled,
            "Cancelled before execution.",
        ),
        6,
    );
    let rows: Vec<String> = block::lines(&s.transcript, 80, None, 0, true)
        .iter()
        .map(plain)
        .collect();
    let text = rows.join("\n");
    assert!(text.contains("write     notes.txt"), "{text}");
    assert!(text.contains("✗ denied"));
    assert!(text.contains("Denied by the user."));
    assert!(text.contains("read"));
    assert!(text.contains("✗ cancelled"));
    // A denial is not a failure: no PEEK banner.
    assert_eq!(s.promotion, p1_tui::state::Promotion::None);
}

#[test]
fn a_cancelled_turn_is_marked_and_leaves_nothing_running() {
    let mut s = Screen::new(true);
    s.apply(&AgentEvent::TurnStarted, 1_000);
    s.apply(&AgentEvent::ReasoningDelta { text: "hmm".into() }, 1_000);
    s.apply(
        &AgentEvent::TextDelta {
            text: "partial ans".into(),
        },
        2_500,
    );
    s.apply(
        &AgentEvent::ToolStarted {
            call: ToolCall {
                call_id: "c1".into(),
                name: "shell".into(),
                input: ToolInput::Json(r#"{"command":"sleep 9"}"#.into()),
            },
        },
        2_600,
    );
    s.apply(
        &AgentEvent::TurnFinished {
            end: TurnEnd::Cancelled,
        },
        4_200,
    );
    let text: Vec<String> = block::lines(&s.transcript, 80, None, 0, true)
        .iter()
        .map(plain)
        .collect();
    let text = text.join("\n");
    assert!(text.contains("· reasoning 1.5s"), "{text}");
    assert!(text.contains("· cancelled after 3.2s"), "{text}");
    assert!(text.contains("✗ cancelled"));
    assert!(!text.contains('▪'));
    assert!(s.working.is_none());
}

#[test]
fn the_working_row_is_ground_text_with_the_current_phase() {
    let mut s = Screen::new(true);
    s.transcript.operator("go");
    s.apply(&AgentEvent::TurnStarted, 0);
    let rows = render(&mut s, 100, 20);
    let row = rows.iter().find(|r| r.contains('▪')).unwrap();
    assert!(row.contains("▪▪▪ waiting for the model"), "{row}");
    assert!(!row.contains("assistant"));
    s.apply(&AgentEvent::TextDelta { text: "hi".into() }, 10);
    let rows = render(&mut s, 100, 20);
    assert!(rows.iter().any(|r| r.contains("▪▪▪ writing")));
}

#[test]
fn resumed_dangling_calls_settle_instead_of_animating() {
    use p1_contracts::{AssistantBlock, AssistantItem, Item};
    let mut s = Screen::new(true);
    s.transcript.paint_history(&[
        Item::User {
            text: "run it".into(),
        },
        Item::Assistant(AssistantItem {
            origin: p1_contracts::Origin {
                route: "r".into(),
                model: "m".into(),
            },
            blocks: vec![AssistantBlock::ToolCall(ToolCall {
                call_id: "d1".into(),
                name: "shell".into(),
                input: ToolInput::Json(r#"{"command":"sleep 30"}"#.into()),
            })],
        }),
    ]);
    let text: String = block::lines(&s.transcript, 80, None, 0, true)
        .iter()
        .map(plain)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("✗ outcome unknown"), "{text}");
    assert!(!text.contains('▪'));
    // The host's later reconciliation updates the same row, never adds one.
    s.apply(
        &result("d1", "shell", ToolStatus::Unknown, "outcome unknown"),
        1,
    );
    assert_eq!(
        s.transcript
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Call(_)))
            .count(),
        1
    );
}

#[test]
fn an_approval_sits_in_the_transcript_with_its_context_and_the_draft() {
    use p1_tui::render::permission::PermissionView;
    let mut s = Screen::new(true);
    s.transcript.operator("clean the build output");
    s.apply(&AgentEvent::TurnStarted, 0);
    s.apply(
        &AgentEvent::TextDelta {
            text: "I will remove the target directory.".into(),
        },
        1,
    );
    s.composer.insert_text("and then rebuild");
    s.approval = Some(p1_tui::state::Approval::Permission(PermissionView {
        tool: "shell".into(),
        command: "rm -rf target/".into(),
        rows: vec![("cwd".into(), "~/p1".into())],
        grantable: true,
    }));
    let rows = render(&mut s, 120, 30);
    let text = rows.join("\n");
    assert!(text.contains("› clean the build output"), "{text}");
    assert!(text.contains("I will remove the target directory."));
    assert!(text.contains("! shell     rm -rf target/"));
    assert!(
        text.contains("› and then rebuild"),
        "the draft stays visible"
    );
    let header = rows.iter().position(|r| r.contains("! shell")).unwrap();
    assert!(
        rows[header + 1].contains("command  rm -rf target/"),
        "the full command is shown"
    );
    let cwd = rows
        .iter()
        .position(|r| r.contains("cwd      ~/p1"))
        .unwrap();
    assert!(
        rows[cwd + 1].contains(" y  allow once"),
        "decisions sit right under the block"
    );
    assert!(
        !rows.iter().any(|r| r.contains("SPEND")),
        "the pane is hidden: full width"
    );
    // Every decision stays visible at narrow widths: rows wrap, nothing is cut.
    for width in 30..=80 {
        let rows = render(&mut s, width, 30).join("\n");
        assert!(
            rows.contains(" n  deny") || rows.contains(" n  "),
            "deny at {width}"
        );
        assert!(rows.contains(" y  "), "allow once at {width}");
        assert!(rows.contains(" a  "), "session at {width}");
    }
}

#[test]
fn scrolling_an_approval_leaves_the_transcript_following_afterwards() {
    use p1_tui::render::diff::DiffView;
    let mut s = mixed();
    s.apply(&AgentEvent::TurnStarted, 0);
    let content: String = (0..80).map(|n| format!("line {n}\n")).collect();
    s.approval = Some(p1_tui::state::Approval::Diff(DiffView::from_write(
        "write", "big.txt", &content, None,
    )));
    render(&mut s, 100, 24);
    s.scroll_by(10);
    render(&mut s, 100, 24);
    assert!(s.scroll_top.is_some());
    s.approval = None;
    s.apply(
        &AgentEvent::TurnFinished {
            end: TurnEnd::Completed {
                stop: p1_contracts::StopReason::EndTurn,
            },
        },
        5,
    );
    render(&mut s, 100, 24);
    assert_eq!(
        s.scroll_top, None,
        "the view re-follows once the approval is gone"
    );
}

#[test]
fn a_resumed_journal_paints_what_the_operator_saw() {
    use p1_contracts::{
        AssistantBlock, AssistantItem, InboxKind, InterruptionReason, JournalRecord, Origin,
        RecordBody, StopReason, Usage,
    };
    let origin = Origin {
        route: "r".into(),
        model: "m".into(),
    };
    let body = |seq, body| JournalRecord { seq, body };
    let records = vec![
        body(
            0,
            RecordBody::UserInput {
                text: "run it".into(),
            },
        ),
        body(
            1,
            RecordBody::AssistantCompleted {
                item: AssistantItem {
                    origin: origin.clone(),
                    blocks: vec![AssistantBlock::ToolCall(ToolCall {
                        call_id: "c1".into(),
                        name: "shell".into(),
                        input: ToolInput::Json(r#"{"command":"echo hi"}"#.into()),
                    })],
                },
                stop: StopReason::ToolUse,
                usage: Some(Usage {
                    input_uncached: Some(100),
                    output: Some(10),
                    ..Usage::default()
                }),
            },
        ),
        body(
            2,
            RecordBody::ToolFinished {
                result: ToolResultItem {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    status: ToolStatus::Ok,
                    content: "hi\n[exit code: 0]".into(),
                },
            },
        ),
        body(
            3,
            RecordBody::Inbox {
                kind: InboxKind::Steering,
                text: "be brief".into(),
            },
        ),
        body(
            4,
            RecordBody::AssistantInterrupted {
                reason: InterruptionReason::Cancelled,
                partial_text: "half an ans".into(),
                error: None,
            },
        ),
    ];
    let mut s = Screen::new(true);
    let usage = s.transcript.replay(&records);
    // The cut response was billed without a report: its usage is unknown.
    assert_eq!(usage.len(), 2);
    assert!(usage[0].is_some() && usage[1].is_none());
    let text: String = block::lines(&s.transcript, 80, None, 0, true)
        .iter()
        .map(plain)
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "› run it",
        "echo hi",
        "✓",
        "› be brief",
        "half an ans",
        "· cancelled",
    ] {
        assert!(text.contains(expected), "{expected} missing:\n{text}");
    }
}

#[test]
fn no_panic_at_any_scroll_offset_size_or_before_a_frame() {
    // A running call below a long history: every scroll offset, including the
    // one whose last visible row is the separator above the running header.
    let mut s = mixed();
    s.apply(&AgentEvent::TurnStarted, 0);
    s.apply(
        &AgentEvent::ToolStarted {
            call: ToolCall {
                call_id: "run".into(),
                name: "shell".into(),
                input: ToolInput::Json(r#"{"command":"sleep 9"}"#.into()),
            },
        },
        1,
    );
    render(&mut s, 100, 30);
    let total = s.last_rendered.0;
    for top in 0..total {
        s.scroll_top = Some(top);
        render(&mut s, 100, 30);
    }
    // Blocks appended since the last frame: selection must not read stale geometry.
    s.transcript.operator("appended after the frame");
    assert!(s.select_first());
    // An approval in terminals from tiny to small.
    s.approval = Some(p1_tui::state::Approval::Permission(
        p1_tui::render::permission::PermissionView {
            tool: "shell".into(),
            command: "rm -rf target/".into(),
            rows: vec![("cwd".into(), "~/p1".into())],
            grantable: true,
        },
    ));
    for h in 1..14 {
        for w in [5, 20, 40, 80] {
            render(&mut s, w, h);
        }
    }
}

#[test]
fn find_highlights_follow_layout_changes_and_esc_state_clears() {
    let mut s = mixed();
    render(&mut s, 100, 30);
    assert!(s.find_next("aligned"));
    let before = s.search_row().unwrap();
    // Expand a block above the match: the match's row moves with its block.
    let header = s
        .transcript
        .blocks
        .iter()
        .position(|b| matches!(b, Block::Reasoning { .. }))
        .unwrap();
    s.transcript.toggle_reasoning_at(header);
    render(&mut s, 100, 30);
    let after = s.search_row().unwrap();
    assert!(after > before, "{before} -> {after}");
    let rows = render(&mut s, 100, 30);
    assert!(rows.iter().any(|r| r.contains("aligned")));
}

#[test]
fn long_prompts_fold_and_prose_paragraphs_join() {
    let mut s = Screen::new(true);
    s.transcript.operator(
        (0..40)
            .map(|n| format!("pasted line {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    s.apply(
        &AgentEvent::TextDelta {
            text: "A paragraph written with\nhard line breaks in the\nmiddle of sentences.\n\n- item one\n- item two".into(),
        },
        1,
    );
    let rows: Vec<String> = block::lines(&s.transcript, 100, None, 0, true)
        .iter()
        .map(plain)
        .collect();
    let text = rows.join("\n");
    assert!(text.contains("pasted line 7") && !text.contains("pasted line 8"));
    assert!(text.contains("· 32 more lines — click to show"), "{text}");
    assert!(text.contains("A paragraph written with hard line breaks in the middle of sentences."));
    assert!(text.contains("- item one") && text.contains("- item two"));
}

#[test]
fn a_recalled_slash_command_does_not_trap_history() {
    let mut s = Screen::new(true);
    s.remember_input("/status");
    s.remember_input("second prompt");
    s.composer.insert_text("my draft");
    s.edit(p1_tui::state::EditAction::Up);
    s.edit(p1_tui::state::EditAction::Up);
    assert_eq!(s.composer.text, "/status");
    assert!(s.palette().is_empty(), "browsing history keeps ↑↓");
    s.edit(p1_tui::state::EditAction::Down);
    s.edit(p1_tui::state::EditAction::Down);
    assert_eq!(s.composer.text, "my draft");
}

#[test]
fn overlays_fit_every_height_and_the_home_never_paints_over_the_palette() {
    // A fresh session: the palette docks over the home screen, which steps
    // aside instead of painting through the command rows.
    for (w, h) in [(80, 24), (100, 30), (120, 40), (60, 20)] {
        let mut s = Screen::new(true);
        s.composer.insert_text("/");
        let rows = render(&mut s, w, h);
        assert!(rows.iter().any(|r| r.contains("/help")));
        assert!(
            rows.iter()
                .all(|r| !r.contains('●') && !r.contains("phaseone")),
            "{w}x{h}: {rows:#?}"
        );
    }
    // Any overlay at any height: no panic, and a picker's inverted row is
    // always the one on screen.
    for h in 1..16 {
        for w in [20, 40, 80] {
            let mut s = mixed();
            s.composer.insert_text("/");
            render(&mut s, w, h);
            s.palette_selected = 9;
            render(&mut s, w, h);
            s.composer.take();
            s.status = Some(vec![p1_tui::render::status::StatusGroup {
                header: "KEYS".into(),
                rows: (0..30)
                    .map(|n| p1_tui::render::status::StatusRow {
                        label: format!("key {n}"),
                        value: "what it does".into(),
                        available: true,
                    })
                    .collect(),
            }]);
            render(&mut s, w, h);
            s.overlay_scroll = 99;
            render(&mut s, w, h);
        }
    }
}

#[test]
fn a_new_overlay_opens_at_its_top_and_follows_the_live_tail_above_it() {
    let mut s = mixed();
    let status = |n: usize| {
        Some(vec![p1_tui::render::status::StatusGroup {
            header: "KEYS".into(),
            rows: (0..n)
                .map(|n| p1_tui::render::status::StatusRow {
                    label: format!("key {n}"),
                    value: "what it does".into(),
                    available: true,
                })
                .collect(),
        }])
    };
    s.status = status(40);
    render(&mut s, 80, 20);
    s.overlay_scroll = 12;
    render(&mut s, 80, 20);
    s.status = None;
    render(&mut s, 80, 20);
    s.status = status(40);
    let rows = render(&mut s, 80, 20);
    assert!(rows.iter().any(|r| r.trim() == "KEYS"), "{rows:#?}");
    // The transcript ends above the overlay: its newest row is still shown.
    s.status = None;
    let rows = render(&mut s, 80, 30);
    let composer = rows
        .iter()
        .position(|r| r.trim_start().starts_with('›'))
        .unwrap();
    let newest = rows[..composer]
        .iter()
        .rev()
        .find(|r| !r.trim().is_empty())
        .unwrap()
        .clone();
    s.status = status(3);
    let rows = render(&mut s, 80, 30);
    let keys = rows.iter().position(|r| r.trim() == "KEYS").unwrap();
    assert!(rows[..keys].contains(&newest), "{newest:?} {rows:#?}");
}

#[test]
fn a_find_hit_follows_its_own_block_and_spans_soft_wraps() {
    let mut s = Screen::new(true);
    s.transcript.operator("go");
    fixture::call(
        &mut s,
        "shell",
        r#"{"command":"seq 60","timeout_seconds":60}"#,
        &(0..60)
            .map(|n| format!("row {n:02}: output\n"))
            .collect::<String>(),
    );
    render(&mut s, 100, 30);
    assert!(s.find_next("row 45"));
    let row = s.search_row().unwrap();
    let rows = render(&mut s, 100, 30);
    let top = s
        .scroll_top
        .unwrap_or(s.last_rendered.0.saturating_sub(s.last_rendered.1));
    assert!(rows[1 + row - top].contains("row 45"), "{rows:#?}");
    // Expanding the matched block itself moves the match: the highlight goes
    // with it, not to whatever sits at the old offset.
    let index = s.transcript.blocks.len() - 1;
    s.toggle_disclosure(index);
    render(&mut s, 100, 30);
    let row = s.search_row().unwrap();
    s.scroll_top = Some(row.saturating_sub(3));
    let rows = render(&mut s, 100, 30);
    let top = s
        .scroll_top
        .unwrap_or(s.last_rendered.0.saturating_sub(s.last_rendered.1));
    assert!(rows[1 + row - top].contains("row 45"), "{rows:#?}");
    // JSON keys of the input are not text the operator reads.
    assert!(!s.find_next("timeout_seconds"));
    // A phrase across a soft wrap is found, on the row where it starts.
    let mut s = Screen::new(true);
    s.apply(
        &AgentEvent::TextDelta {
            text: "The renderer keeps settled rows cached per width, so a resize re-lays out only once.".into(),
        },
        1,
    );
    render(&mut s, 40, 20);
    assert!(s.find_next("per width, so a resize"));
    assert!(s.search_row().is_some());
}

#[test]
fn a_docked_palette_counts_the_commands_it_hides_on_both_sides() {
    let mut s = mixed();
    s.composer.insert_text("/");
    let total = p1_tui::commands::COMMANDS.len();
    for (h, selected) in [(14, 10), (16, 0), (18, total - 1)] {
        s.palette_selected = selected;
        let rows = render(&mut s, 80, h);
        let shown = rows
            .iter()
            .filter(|r| r.trim_start().starts_with('/') && !r.contains('›'))
            .count();
        let footer = rows.iter().find(|r| r.contains("↑↓ select")).cloned();
        let count = |mark: char| {
            footer.as_deref().map_or(0, |f| {
                f.split(mark)
                    .nth(1)
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or(0)
            })
        };
        assert_eq!(
            shown + count('↑') + count('↓'),
            total,
            "{h} rows, selection {selected}: {rows:#?}"
        );
        // The selected command is one of those shown.
        let name = format!("/{}", p1_tui::commands::COMMANDS[selected].name);
        assert!(rows.iter().any(|r| r.contains(&name)), "{rows:#?}");
    }
}
