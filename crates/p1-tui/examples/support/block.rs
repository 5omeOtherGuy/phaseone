use p1_contracts::{AgentEvent, ToolCall, ToolInput, ToolResultItem, ToolStatus};
use p1_tui::{
    render::ledger::{Context, Task},
    state::Screen,
    transcript::Block,
};
pub fn call(s: &mut Screen, name: &str, input: &str, output: &str) {
    let id = format!("c{}", s.transcript.blocks.len());
    s.transcript.apply(
        &AgentEvent::ToolStarted {
            call: ToolCall {
                call_id: id.clone(),
                name: name.into(),
                input: ToolInput::Json(input.into()),
            },
        },
        None,
    );
    s.transcript.apply(
        &AgentEvent::ToolFinished {
            result: ToolResultItem {
                call_id: id,
                name: name.into(),
                status: ToolStatus::Ok,
                content: output.into(),
            },
        },
        Some(420),
    );
}
pub fn screen(name: &str) -> Screen {
    let mut s = Screen::new(true);
    s.repo = "phaseone".into();
    s.branch = "main".into();
    s.model = "Fable 5.1".into();
    s.effort = "high".into();
    s.goal = Some("Implement BLOCK and verify the real workflow".into());
    s.context_view = Some(Context {
        used: 12_400,
        window: 200_000,
        warn_at: 120_000,
        parts: vec![],
    });
    s.task_view = Some(Task {
        id: None,
        files: Some(3),
        diff: Some((48, 12)),
        journal: Some("saved".into()),
    });
    match name {
        "read" => call(
            &mut s,
            "read",
            r#"{"file_path":"src/main.rs"}"#,
            "     1\tfn main() {\n     2\t    println!(\"hello\");\n     3\t}",
        ),
        "shell" => call(
            &mut s,
            "shell",
            r#"{"command":"cargo test -p p1-tui","cwd":"~/phaseone"}"#,
            "running 3 tests\ntest composer ... ok\ntest output ... FAILED\nassertion failed: expected 24 rows\n[exit code: 101]",
        ),
        "write" => call(
            &mut s,
            "write",
            r#"{"file_path":"src/new.rs","content":"pub fn answer() -> u8 {\n    42\n}\n"}"#,
            "Wrote src/new.rs (35 bytes).",
        ),
        "edit" => call(
            &mut s,
            "edit",
            r#"{"file_path":"src/lib.rs","old_string":"let limit = 8;","new_string":"let limit = 24;"}"#,
            "Replaced 1 occurrence.",
        ),
        "skill" => call(
            &mut s,
            "skill",
            r#"{"name":"model-cards"}"#,
            "3 cards · deepseek · kimi · glm\nevidence.jsonl 41 rows",
        ),
        "search" => call(
            &mut s,
            "search",
            r#"{"pattern":"render"}"#,
            "screen.rs — compose visible cells\nblock.rs — render tool output",
        ),
        "send" => call(
            &mut s,
            "send",
            r#"{"id":"worker-1"}"#,
            "Verify the narrow layout\nCheck all four insets\nKeep the outcome visible",
        ),
        "stop" => call(
            &mut s,
            "stop",
            r#"{"id":"worker-1"}"#,
            "local agent · 6 calls spent · worktree kept",
        ),
        "delegate" => call(
            &mut s,
            "delegate",
            r#"{"ceiling":40,"workers":[{"state":"▪","name":"renderer","route":"deepseek","profile":"v4.1-flash","owns":"crates/p1-tui/**","activity":"running width sweep","elapsed_ms":72000},{"state":"✓","name":"input","route":"kimi","profile":"k3","owns":"crates/p1-host/src/tui.rs","activity":"paste handling complete","elapsed_ms":130000}]}"#,
            "Two workers started.",
        ),
        "ask" => call(
            &mut s,
            "ask",
            r#"{"question":"Which layout?","focused":1,"options":[{"label":"Compact","description":"More transcript space"},{"label":"Expanded","description":"Full tool evidence"}]}"#,
            "Pick one.",
        ),
        "notify" => call(
            &mut s,
            "notify",
            "renderer returned",
            "verdict  Width sweep passed at 80–200 columns\nfiles    3 written, all inside owned paths\n↳ review staged worktree before merge",
        ),
        "compact" => call(
            &mut s,
            "compact",
            "session scope only",
            "kept     Design decisions · open tasks\ndropped  Completed sidequests",
        ),
        "fold" => call(
            &mut s,
            "shell",
            r#"{"command":"cargo test --workspace"}"#,
            &((0..4000)
                .map(|n| format!("test {n:04} ... ok\n"))
                .collect::<String>()
                + "[exit code: 0]"),
        ),
        "approval" => {
            s.approval = Some(p1_tui::state::Approval::Diff(
                p1_tui::render::diff::DiffView::from_edit(
                    "edit",
                    "src/renderer.rs",
                    "let keep = 8;",
                    "let keep = 24;",
                    Some("fn render() {\nlet keep = 8;\n}"),
                    (1, 1),
                ),
            ));
        }
        "idle" => {}
        _ => {
            s.transcript
                .operator("Read the renderer and check the failing test.");
            call(
                &mut s,
                "read",
                r#"{"file_path":"src/renderer.rs"}"#,
                "     1\tfn output_limit() -> usize {\n     2\t    24\n     3\t}",
            );
            call(
                &mut s,
                "shell",
                r#"{"command":"cargo test -p p1-tui","cwd":"~/phaseone"}"#,
                "running 3 tests\ntest multiline_input ... ok\ntest output_navigation ... ok\ntest viewport_cache ... ok\n\ntest result: ok. 3 passed; 0 failed\n[exit code: 0]",
            );
            s.transcript.blocks.push(Block::Prose{lines:vec!["The renderer passes. Tool evidence stays expanded; oversized output opens in the pane without moving your scroll position.".into()]});
            s.composer
                .insert_text("Now check the 80-column layout.\nKeep the output pane reachable.");
        }
    }
    s
}
