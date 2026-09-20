//! Lead check on dogfood run read-long-line-gpt-1: valid multi-byte text longer than
//! the read buffer must be accepted wherever the chunk boundaries fall, and an invalid
//! byte after a split character must still be rejected.

use p1_contracts::{CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolStatus};
use p1_tool_read::ReadTool;
use p1_workspace::{ObservedFiles, Workspace};

async fn read(root: &std::path::Path, name: &str) -> p1_contracts::ToolOutcome {
    let tool = ReadTool::new(Workspace::new(root).unwrap(), ObservedFiles::new());
    let call = ToolCall {
        call_id: "c".into(),
        name: "read".into(),
        input: ToolInput::Json(format!(r#"{{"file_path":"{name}"}}"#)),
    };
    tool.execute(
        &call,
        ToolContext {
            cancel: CancellationToken::new(),
        },
    )
    .await
}

#[tokio::test]
async fn consecutive_three_byte_characters_are_valid_at_every_alignment() {
    let dir = tempfile::tempdir().unwrap();
    for shift in 0..3 {
        let long_line = format!("{}{}\n", "a".repeat(shift), "€".repeat(100_000));
        std::fs::write(dir.path().join("long.txt"), &long_line).unwrap();
        let outcome = read(dir.path(), "long.txt").await;
        assert_eq!(
            outcome.status,
            ToolStatus::Ok,
            "long line, shift {shift}: {}",
            outcome.content
        );

        let many_lines = format!("{}{}", "a".repeat(shift), "€€€€€€€€€\n".repeat(30_000));
        std::fs::write(dir.path().join("lines.txt"), &many_lines).unwrap();
        let outcome = read(dir.path(), "lines.txt").await;
        assert_eq!(
            outcome.status,
            ToolStatus::Ok,
            "many lines, shift {shift}: {}",
            outcome.content
        );
    }
}

#[tokio::test]
async fn an_invalid_byte_after_a_split_character_is_still_rejected() {
    let dir = tempfile::tempdir().unwrap();
    for shift in 0..3 {
        let mut bytes = format!("{}{}", "a".repeat(shift), "€".repeat(100_000)).into_bytes();
        bytes.push(0xFF);
        bytes.extend_from_slice("€€€\n".as_bytes());
        std::fs::write(dir.path().join("bad.txt"), &bytes).unwrap();
        let outcome = read(dir.path(), "bad.txt").await;
        assert_eq!(outcome.status, ToolStatus::Error, "shift {shift}");
        assert!(
            outcome.content.contains("not valid UTF-8"),
            "{}",
            outcome.content
        );
    }
}
