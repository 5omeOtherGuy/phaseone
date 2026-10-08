//! ADR-0125 (issue #418): `read` takes several files or ranges in one call with `files`.
//! Each entry renders exactly as a single read of it, under its own `==> path <==` header;
//! the call keeps the single read's one byte limit; one entry's error never stops the
//! others; and each entry is observed exactly as a single read of it is.
//!
//! The changes go through `Workspace::commit` under `MutationPolicy::Observed`, the
//! read-before-mutate check edit and write run.

use p1_contracts::{CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolStatus};
use p1_tool_read::ReadTool;
use p1_workspace::{Change, MutationPolicy, ObservedFiles, Workspace};

const NOT_READ: &str = "not read: this call's output limit was reached; read it in another call";

fn setup(root: &std::path::Path) -> (ReadTool, Workspace, ObservedFiles) {
    let workspace = Workspace::new(root).unwrap();
    let observed = ObservedFiles::new();
    let read = ReadTool::new(workspace.clone(), observed.clone());
    (read, workspace, observed)
}

async fn read(tool: &ReadTool, json: &str) -> p1_contracts::ToolOutcome {
    let call = ToolCall {
        call_id: "c".into(),
        name: "read".into(),
        input: ToolInput::Json(json.into()),
    };
    tool.execute(
        &call,
        ToolContext {
            cancel: CancellationToken::new(),
        },
    )
    .await
}

fn change(workspace: &Workspace, observed: &ObservedFiles, path: &str) -> Result<(), String> {
    workspace
        .commit(
            &[Change::write(path, "changed\n")],
            observed,
            MutationPolicy::Observed,
        )
        .map_err(|error| error.to_string())
}

fn section(path: &str, body: &str) -> String {
    format!("==> {path} <==\n{body}")
}

/// `a.txt` = "one\ntwo\n", `b.txt` = 3000 lines `line N`, no `c.txt`.
fn requirement_two_workspace(root: &std::path::Path) {
    std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
    let b: String = (1..=3000).map(|n| format!("line {n}\n")).collect();
    std::fs::write(root.join("b.txt"), b).unwrap();
}

const REQUIREMENT_TWO: &str = r#"{"files":[{"file_path":"a.txt"},{"file_path":"b.txt","offset":2999,"limit":5},{"file_path":"c.txt"}]}"#;

#[tokio::test]
async fn each_entry_renders_as_its_single_read_and_one_missing_file_stops_nothing() {
    let dir = tempfile::tempdir().unwrap();
    requirement_two_workspace(dir.path());
    let (tool, workspace, observed) = setup(dir.path());

    let several = read(&tool, REQUIREMENT_TWO).await;

    // The single reads of the same entries, each in a workspace of its own so their
    // observations do not stand in for the call's.
    let single_dir = tempfile::tempdir().unwrap();
    requirement_two_workspace(single_dir.path());
    let (single, _, _) = setup(single_dir.path());
    let a = read(&single, r#"{"file_path":"a.txt"}"#).await;
    let b = read(&single, r#"{"file_path":"b.txt","offset":2999,"limit":5}"#).await;
    let c = read(&single, r#"{"file_path":"c.txt"}"#).await;
    assert_eq!(a.status, ToolStatus::Ok);
    assert_eq!(b.content, "  2999\tline 2999\n  3000\tline 3000");
    assert_eq!(c.status, ToolStatus::Error);
    assert_eq!(c.content, "c.txt does not exist.");

    assert_eq!(several.status, ToolStatus::Ok, "{}", several.content);
    assert_eq!(
        several.content,
        [
            section("a.txt", &a.content),
            section("b.txt", &b.content),
            section("c.txt", &c.content),
        ]
        .join("\n\n")
    );

    // a.txt was read whole: a write without another read is accepted.
    assert_eq!(change(&workspace, &observed, "a.txt"), Ok(()));
    // b.txt was read in part: the write follows what a single partial read allows.
    let partial_dir = tempfile::tempdir().unwrap();
    requirement_two_workspace(partial_dir.path());
    let (partial, partial_workspace, partial_observed) = setup(partial_dir.path());
    read(&partial, r#"{"file_path":"b.txt","offset":2999,"limit":5}"#).await;
    // A single windowed read observes the whole file, so both writes are accepted.
    assert_eq!(
        change(&partial_workspace, &partial_observed, "b.txt"),
        Ok(())
    );
    assert_eq!(change(&workspace, &observed, "b.txt"), Ok(()));
}

#[tokio::test]
async fn a_call_gives_file_path_or_files_and_files_holds_one_to_ten_known_entries() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    let (tool, _, _) = setup(dir.path());

    for both_or_neither in [
        r#"{"file_path":"a.txt","files":[{"file_path":"a.txt"}]}"#,
        r#"{}"#,
        r#"{"offset":1}"#,
    ] {
        let outcome = read(&tool, both_or_neither).await;
        assert_eq!(outcome.status, ToolStatus::Error, "{both_or_neither}");
        assert!(
            outcome.content.starts_with("Invalid input for read: ")
                && outcome.content.contains("`file_path`")
                && outcome.content.contains("`files`"),
            "{both_or_neither}: {}",
            outcome.content
        );
    }

    let eleven = format!(
        r#"{{"files":[{}]}}"#,
        vec![r#"{"file_path":"a.txt"}"#; 11].join(",")
    );
    for invalid in [
        r#"{"files":[]}"#.to_string(),
        eleven,
        r#"{"files":[{"file_path":"a.txt","unknown":1}]}"#.to_string(),
        r#"{"files":[{"file_path":"a.txt","offset":0}]}"#.to_string(),
        r#"{"files":[{"offset":1}]}"#.to_string(),
        r#"{"files":[{"file_path":"a.txt"}],"offset":1}"#.to_string(),
    ] {
        let outcome = read(&tool, &invalid).await;
        assert_eq!(outcome.status, ToolStatus::Error, "{invalid}");
        assert!(
            outcome.content.starts_with("Invalid input for read: "),
            "{invalid}: {}",
            outcome.content
        );
    }

    let ten = format!(
        r#"{{"files":[{}]}}"#,
        vec![r#"{"file_path":"a.txt"}"#; 10].join(",")
    );
    assert_eq!(read(&tool, &ten).await.status, ToolStatus::Ok);
}

/// 300 lines of 99 bytes and a newline: 30,000 bytes.
fn thirty_thousand_bytes(letter: char) -> String {
    (0..300)
        .map(|_| format!("{}\n", letter.to_string().repeat(99)))
        .collect()
}

#[tokio::test]
async fn the_call_keeps_one_byte_limit_and_names_every_entry_it_could_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let (first, second, third) = (
        thirty_thousand_bytes('a'),
        thirty_thousand_bytes('b'),
        thirty_thousand_bytes('c'),
    );
    for (name, contents) in [("1.txt", &first), ("2.txt", &second), ("3.txt", &third)] {
        assert_eq!(contents.len(), 30_000);
        std::fs::write(dir.path().join(name), contents).unwrap();
    }
    // The oracle: a single read of the first two files as one file cuts where the call's
    // limit must cut the second.
    std::fs::write(dir.path().join("joined.txt"), format!("{first}{second}")).unwrap();
    let (tool, _, _) = setup(dir.path());

    let several = read(
        &tool,
        r#"{"files":[{"file_path":"1.txt"},{"file_path":"2.txt"},{"file_path":"3.txt"}]}"#,
    )
    .await;
    let joined = read(&tool, r#"{"file_path":"joined.txt"}"#).await;
    let joined_lines = joined
        .content
        .lines()
        .filter(|line| !line.starts_with('['))
        .count();
    assert!(joined_lines > 300 && joined_lines < 600, "{joined_lines}");
    let whole_first = read(&tool, r#"{"file_path":"1.txt"}"#).await;
    let cut_second = read(
        &tool,
        &format!(r#"{{"file_path":"2.txt","limit":{}}}"#, joined_lines - 300),
    )
    .await;

    assert_eq!(several.status, ToolStatus::Ok);
    assert_eq!(
        several.content,
        [
            section("1.txt", &whole_first.content),
            section("2.txt", &cut_second.content),
            section("3.txt", NOT_READ),
        ]
        .join("\n\n")
    );
    // The second section ends with its next-offset line, as a single read's window does.
    assert!(
        cut_second
            .content
            .ends_with(&format!("continue with offset={}]", joined_lines - 300 + 1)),
        "{}",
        cut_second.content
    );
    // Rendered lines stay within the single read's 50,000 bytes: only headers, the
    // separators, the next-offset line and the budget line come on top.
    let footer = cut_second.content.lines().last().unwrap();
    let overhead =
        "==> 1.txt <==\n".len() * 3 + "\n\n".len() * 2 + "\n".len() + footer.len() + NOT_READ.len();
    assert!(
        several.content.len() <= 50_000 + overhead,
        "{} > 50,000 + {overhead}",
        several.content.len()
    );
}

#[tokio::test]
async fn a_call_is_an_error_only_when_every_entry_failed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("subdir")).unwrap();
    std::fs::write(dir.path().join("nul.dat"), b"alpha\0beta").unwrap();
    let (tool, _, _) = setup(dir.path());

    let several = read(
        &tool,
        r#"{"files":[{"file_path":"nope.txt"},{"file_path":"subdir"},{"file_path":"nul.dat"},{"file_path":"../outside.txt"}]}"#,
    )
    .await;

    let mut sections = Vec::new();
    for path in ["nope.txt", "subdir", "nul.dat", "../outside.txt"] {
        let single = read(&tool, &format!(r#"{{"file_path":"{path}"}}"#)).await;
        assert_eq!(single.status, ToolStatus::Error, "{path}");
        sections.push(section(path, &single.content));
    }
    assert_eq!(several.status, ToolStatus::Error);
    assert_eq!(several.content, sections.join("\n\n"));
}

#[tokio::test]
async fn every_entry_is_refused_and_confined_as_its_single_read() {
    let home = tempfile::tempdir().unwrap();
    let credential = home.path().join(".codex/auth.json");
    std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
    std::fs::write(&credential, "{}\n").unwrap();
    std::fs::write(home.path().join("notes.txt"), "alpha\n").unwrap();
    let workspace = Workspace::new(home.path()).unwrap();
    let tool =
        ReadTool::new(workspace, ObservedFiles::new()).with_home(Some(home.path().to_path_buf()));

    let several = read(
        &tool,
        r#"{"files":[{"file_path":"notes.txt"},{"file_path":".codex/auth.json"},{"file_path":"../outside.txt"}]}"#,
    )
    .await;

    assert_eq!(several.status, ToolStatus::Ok);
    let refused = read(&tool, r#"{"file_path":".codex/auth.json"}"#).await;
    assert!(refused.content.contains("read refuses credential files"));
    let escaped = read(&tool, r#"{"file_path":"../outside.txt"}"#).await;
    assert!(escaped.content.contains("escapes workspace"));
    assert_eq!(
        several.content,
        [
            section("notes.txt", "     1\talpha"),
            section(".codex/auth.json", &refused.content),
            section("../outside.txt", &escaped.content),
        ]
        .join("\n\n")
    );
    assert!(!several.content.contains("{}"));
}

#[tokio::test]
async fn each_entry_is_observed_as_its_single_read_and_a_skimmed_one_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let source =
        "// a comment long enough that hiding it saves more bytes than the note costs\nfn f() {}\n";
    std::fs::write(dir.path().join("full.rs"), source).unwrap();
    std::fs::write(dir.path().join("skimmed.rs"), source).unwrap();
    let (tool, workspace, observed) = setup(dir.path());

    let several = read(
        &tool,
        r#"{"files":[{"file_path":"full.rs"},{"file_path":"skimmed.rs","skim":true}]}"#,
    )
    .await;

    assert_eq!(several.status, ToolStatus::Ok);
    assert!(
        several.content.contains("[skim: 1 lines hidden"),
        "{}",
        several.content
    );
    assert_eq!(change(&workspace, &observed, "full.rs"), Ok(()));
    assert!(change(&workspace, &observed, "skimmed.rs").is_err());
}

#[test]
fn the_declaration_offers_files_and_says_to_batch_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let (tool, _, _) = setup(dir.path());
    let declaration = tool.declaration();
    assert!(declaration.description.contains("`files`"));
    assert!(
        declaration
            .description
            .contains("rather than one call each")
    );
    let p1_contracts::DeclarationKind::Function { input_schema } = &declaration.kind else {
        panic!("a function declaration");
    };
    let files = &input_schema["properties"]["files"];
    assert_eq!(files["type"], "array");
    assert_eq!(files["minItems"], 1);
    assert_eq!(files["maxItems"], 10);
    assert_eq!(files["items"]["required"], serde_json::json!(["file_path"]));
    assert_eq!(files["items"]["additionalProperties"], false);
}

#[test]
fn the_call_names_every_entry_it_reads() {
    let dir = tempfile::tempdir().unwrap();
    let (tool, _, _) = setup(dir.path());
    let call = ToolCall {
        call_id: "c".into(),
        name: "read".into(),
        input: ToolInput::Json(REQUIREMENT_TWO.into()),
    };
    assert_eq!(
        tool.describe(&call).target.as_deref(),
        Some("a.txt, b.txt:2999-3003, c.txt")
    );
}
