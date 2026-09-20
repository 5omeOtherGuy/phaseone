//! Lead acceptance for `grep` bounding its own output (docs/design/tools.md,
//! research #36): a result over the shared output bound is cut by the tool
//! itself, never mid-block, and a footer says what is missing and how to get
//! it.
//!
//! The footers are written out literally here, straight from the spec
//! paragraph, so a change in wording shows up as a failure rather than as a
//! matching change in the implementation.

use std::path::Path;

use p1_contracts::{
    CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
};
use p1_tool_search::{GrepTool, MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES};
use p1_workspace::Workspace;

fn grep_tool(root: &Path) -> GrepTool {
    GrepTool::new(Workspace::new(root).unwrap())
}

async fn execute(tool: &GrepTool, arguments: &str) -> ToolOutcome {
    let call = ToolCall {
        call_id: "call-1".into(),
        name: "grep".into(),
        input: ToolInput::Json(arguments.to_string()),
    };
    let context = ToolContext {
        cancel: CancellationToken::new(),
    };
    tool.execute(&call, context).await
}

fn newlines(text: &str) -> usize {
    text.matches('\n').count()
}

/// The shared output bound, for a result that ends with the footer line and has
/// no trailing newline: at most `MAX_OUTPUT_BYTES` bytes and fewer than
/// `MAX_OUTPUT_LINES` newlines, so the footer line itself is inside the bound.
fn assert_within_bound(content: &str) {
    assert!(
        content.len() <= MAX_OUTPUT_BYTES,
        "{} bytes is over the bound",
        content.len()
    );
    assert!(
        newlines(content) < MAX_OUTPUT_LINES,
        "{} newlines is over the bound",
        newlines(content)
    );
}

/// Whether one more unit (a block in content mode, a path in files mode) plus
/// its footer would still fit. The tool must keep units only while this is
/// false for the next one.
fn one_more_fits(body: &str, separator: &str, unit: &str, footer: &str) -> bool {
    let with_next = format!("{body}{separator}{unit}\n{footer}");
    with_next.len() <= MAX_OUTPUT_BYTES && newlines(&with_next) < MAX_OUTPUT_LINES
}

fn content_line() -> String {
    format!("needle {}", "0123456789".repeat(12))
}

fn content_fixture(root: &Path, files: usize) -> String {
    let line = content_line();
    for index in 0..files {
        std::fs::write(root.join(format!("f{index:05}.txt")), format!("{line}\n")).unwrap();
    }
    line
}

const CONTENT_FILES: usize = 1_500;

#[tokio::test]
async fn content_mode_keeps_whole_blocks_and_names_the_last_file_shown() {
    let dir = tempfile::tempdir().unwrap();
    let line = content_fixture(dir.path(), CONTENT_FILES);
    let tool = grep_tool(dir.path());

    let outcome = execute(&tool, r#"{"pattern": "needle"}"#).await;

    assert_eq!(outcome.status, ToolStatus::Ok);
    let content = &outcome.content;
    assert_within_bound(content);
    let (body, footer) = content.rsplit_once('\n').expect("a footer line");
    let blocks: Vec<&str> = body.split("\n\n").collect();
    let shown = blocks.len();
    assert!(shown > 1 && shown < CONTENT_FILES, "shown={shown}");
    // Every kept block is one whole file block, in walk order.
    for (index, block) in blocks.iter().enumerate() {
        assert_eq!(
            *block,
            format!("f{index:05}.txt\n1:{line}"),
            "block {index} is not a whole file block"
        );
    }
    let last = format!("f{:05}.txt", shown - 1);
    assert_eq!(
        footer,
        format!(
            "[truncated after {last}; {} more matching files not shown; narrow with path or glob]",
            CONTENT_FILES - shown
        )
    );
    // The tool stopped only because one more block would not have fit.
    let next = format!("f{shown:05}.txt\n1:{line}");
    let next_footer = format!(
        "[truncated after f{shown:05}.txt; {} more matching files not shown; narrow with path or glob]",
        CONTENT_FILES - shown - 1
    );
    assert!(
        !one_more_fits(body, "\n\n", &next, &next_footer),
        "one more file block would still have fit"
    );
    assert!(!content.contains("[output truncated"));
}

const FILE_MODE_FILES: usize = 2_000;

fn file_mode_name(index: usize) -> String {
    format!("file_{index:04}_needle_padding.txt")
}

#[tokio::test]
async fn files_mode_keeps_whole_paths_and_names_the_last_one_shown() {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..FILE_MODE_FILES {
        std::fs::write(dir.path().join(file_mode_name(index)), "needle\n").unwrap();
    }
    let tool = grep_tool(dir.path());

    let outcome = execute(&tool, r#"{"pattern": "needle", "mode": "files"}"#).await;

    assert_eq!(outcome.status, ToolStatus::Ok);
    let content = &outcome.content;
    assert_within_bound(content);
    let (body, footer) = content.rsplit_once('\n').expect("a footer line");
    let paths: Vec<&str> = body.split('\n').collect();
    let shown = paths.len();
    assert!(shown > 1 && shown < FILE_MODE_FILES, "shown={shown}");
    for (index, path) in paths.iter().enumerate() {
        assert_eq!(*path, file_mode_name(index), "path {index} is not whole");
    }
    assert_eq!(
        footer,
        format!(
            "[truncated after {}; {} more matching files not shown; narrow with path or glob]",
            file_mode_name(shown - 1),
            FILE_MODE_FILES - shown
        )
    );
    let next = file_mode_name(shown);
    let next_footer = format!(
        "[truncated after {next}; {} more matching files not shown; narrow with path or glob]",
        FILE_MODE_FILES - shown - 1
    );
    assert!(
        !one_more_fits(body, "\n", &next, &next_footer),
        "one more path would still have fit"
    );
    assert!(!content.contains("[output truncated"));
}

const BLOCK_LINES: usize = 1_500;

fn block_line() -> String {
    format!("needle {}", "x".repeat(80))
}

#[tokio::test]
async fn a_single_block_over_the_bound_is_cut_at_a_line_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let line = block_line();
    let contents: String = (0..BLOCK_LINES).map(|_| format!("{line}\n")).collect();
    std::fs::write(dir.path().join("big.txt"), contents).unwrap();
    let tool = grep_tool(dir.path());

    let outcome = execute(&tool, r#"{"pattern": "needle"}"#).await;

    assert_eq!(outcome.status, ToolStatus::Ok);
    let content = &outcome.content;
    assert_within_bound(content);
    let (body, footer) = content.rsplit_once('\n').expect("a footer line");
    let mut lines = body.split('\n');
    assert_eq!(lines.next(), Some("big.txt"));
    let hits: Vec<&str> = lines.collect();
    let shown = hits.len();
    assert!(shown > 1 && shown < BLOCK_LINES, "shown={shown}");
    // Cut at a line boundary: every line shown is a whole line of the file.
    for (index, hit) in hits.iter().enumerate() {
        assert_eq!(
            *hit,
            format!("{}:{line}", index + 1),
            "line {} is not whole",
            index + 1
        );
    }
    assert_eq!(
        footer,
        format!(
            "[truncated inside big.txt after line {shown}; 0 more matching files not shown; narrow with path, glob or a stricter pattern]"
        )
    );
    let next = format!("{}:{line}", shown + 1);
    let next_footer = format!(
        "[truncated inside big.txt after line {}; 0 more matching files not shown; narrow with path, glob or a stricter pattern]",
        shown + 1
    );
    assert!(
        !one_more_fits(body, "\n", &next, &next_footer),
        "one more line would still have fit"
    );
    assert!(!content.contains("[output truncated"));
}

#[tokio::test]
async fn a_single_block_over_the_line_cap_is_cut_to_whole_lines() {
    let dir = tempfile::tempdir().unwrap();
    // 2_500 matching lines fit in bytes but not in the line count.
    std::fs::write(dir.path().join("lines.txt"), "needle\n".repeat(2_500)).unwrap();
    let tool = grep_tool(dir.path());

    let outcome = execute(&tool, r#"{"pattern": "needle"}"#).await;

    assert_eq!(outcome.status, ToolStatus::Ok);
    let content = &outcome.content;
    assert_within_bound(content);
    let (body, footer) = content.rsplit_once('\n').expect("a footer line");
    let mut lines = body.split('\n');
    assert_eq!(lines.next(), Some("lines.txt"));
    let hits: Vec<&str> = lines.collect();
    // The path line, the hits and the footer are all inside the 2_000 lines.
    assert_eq!(hits.len(), MAX_OUTPUT_LINES - 2);
    for (index, hit) in hits.iter().enumerate() {
        assert_eq!(*hit, format!("{}:needle", index + 1), "line {}", index + 1);
    }
    assert_eq!(
        footer,
        format!(
            "[truncated inside lines.txt after line {}; 0 more matching files not shown; narrow with path, glob or a stricter pattern]",
            MAX_OUTPUT_LINES - 2
        )
    );
    assert!(!content.contains("[output truncated"));
}

#[tokio::test]
async fn a_result_under_the_bound_is_unchanged_and_carries_no_footer() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("target")).unwrap();
    std::fs::create_dir_all(dir.path().join(".hidden")).unwrap();
    std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
    std::fs::write(dir.path().join("src/b.rs"), "fn beta() {}\n").unwrap();
    std::fs::write(dir.path().join("target/x.rs"), "fn beta() {}\n").unwrap();
    std::fs::write(dir.path().join(".hidden/c.rs"), "fn beta() {}\n").unwrap();
    let tool = grep_tool(dir.path());

    let content = execute(&tool, r#"{"pattern": "beta"}"#).await;
    assert_eq!(content.status, ToolStatus::Ok);
    assert_eq!(
        content.content,
        "src/a.rs\n2:fn beta() {}\n\nsrc/b.rs\n1:fn beta() {}"
    );

    let with_context = execute(&tool, r#"{"pattern": "beta", "context": 1}"#).await;
    assert_eq!(
        with_context.content,
        "src/a.rs\n1-fn alpha() {}\n2:fn beta() {}\n\nsrc/b.rs\n1:fn beta() {}"
    );

    let files = execute(&tool, r#"{"pattern": "beta", "mode": "files"}"#).await;
    assert_eq!(files.content, "src/a.rs\nsrc/b.rs");

    // A result many blocks wide, still under the bound, is the plain join.
    let dir = tempfile::tempdir().unwrap();
    let mut expected = String::new();
    for index in 0..200 {
        std::fs::write(dir.path().join(format!("m{index:03}.txt")), "needle\n").unwrap();
        if index > 0 {
            expected.push_str("\n\n");
        }
        expected.push_str(&format!("m{index:03}.txt\n1:needle"));
    }
    let wide_tool = grep_tool(dir.path());
    let wide = execute(&wide_tool, r#"{"pattern": "needle"}"#).await;

    assert_eq!(wide.status, ToolStatus::Ok);
    assert_eq!(wide.content, expected);
    assert!(!wide.content.contains("[truncated"));
}

/// A single line larger than the whole bound has no line boundary to cut at, so
/// its text is cut — on a character boundary — and the footer still names it.
#[tokio::test]
async fn a_single_line_larger_than_the_bound_is_cut_on_a_character_boundary() {
    let dir = tempfile::tempdir().unwrap();
    // 20_000 three-byte characters: the byte budget lands mid-character.
    let huge = format!("needle{}", "€".repeat(20_000));
    std::fs::write(dir.path().join("huge.txt"), format!("{huge}\n")).unwrap();
    let tool = grep_tool(dir.path());

    let outcome = execute(&tool, r#"{"pattern": "needle"}"#).await;

    assert_eq!(outcome.status, ToolStatus::Ok);
    let content = &outcome.content;
    assert_within_bound(content);
    assert!(content.starts_with("huge.txt\n1:needle€"), "{content:.80}");
    let (_, footer) = content.rsplit_once('\n').expect("a footer line");
    assert_eq!(
        footer,
        "[truncated inside huge.txt after line 1; 0 more matching files not shown; narrow with path, glob or a stricter pattern]"
    );
    assert!(!content.contains("showing"));
}

#[tokio::test]
async fn the_old_bytes_footer_never_appears() {
    let dir = tempfile::tempdir().unwrap();
    let huge = "y".repeat(60_000);
    std::fs::write(dir.path().join("huge.txt"), format!("needle{huge}\n")).unwrap();
    let tool = grep_tool(dir.path());
    let one_file = execute(&tool, r#"{"pattern": "needle"}"#).await;

    let dir = tempfile::tempdir().unwrap();
    content_fixture(dir.path(), CONTENT_FILES);
    let many_tool = grep_tool(dir.path());
    let many_files = execute(&many_tool, r#"{"pattern": "needle"}"#).await;

    let dir = tempfile::tempdir().unwrap();
    for index in 0..2_500 {
        std::fs::write(dir.path().join(file_mode_name(index)), "").unwrap();
    }
    let listing_tool = grep_tool(dir.path());
    let listed = execute(
        &listing_tool,
        r#"{"pattern": "", "mode": "files", "glob": "*.txt"}"#,
    )
    .await;

    for outcome in [&one_file, &many_files, &listed] {
        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_within_bound(&outcome.content);
        assert!(
            !outcome.content.contains("[output truncated"),
            "the old bytes-only footer appeared: {:.120}",
            outcome.content
        );
        assert!(!outcome.content.contains("showing"));
    }
    assert!(listed.content.contains("truncated after file_"));
}
