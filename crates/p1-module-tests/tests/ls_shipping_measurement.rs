//! Offline byte-count measurement: complete declarations, serialized calls and results.
//! No offline tokenizer vocabulary is shipped in this repository; byte counts are not tokens.
use p1_contracts::serde_json::{Value, json};
use p1_contracts::{CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolStatus};
use p1_module_runtime::{Digest, ExecutionLimits, wasm_tool};
use p1_module_tests::Release;
use p1_redact::MaskCounter;
use p1_tool_shell::ShellTool;
use p1_workspace::{ObservedFiles, Workspace};
use std::sync::Arc;

async fn run(tool: &dyn Tool, args: Value) -> (usize, String) {
    let call = ToolCall {
        call_id: "measure".into(),
        name: tool.declaration().name.clone(),
        input: ToolInput::Json(args.to_string()),
    };
    let call_bytes = serde_json::to_vec(&call).unwrap().len();
    let result = tool
        .execute(
            &call,
            ToolContext {
                cancel: CancellationToken::new(),
            },
        )
        .await;
    assert_eq!(result.status, ToolStatus::Ok, "{}", result.content);
    (
        call_bytes
            + json!({"status":"ok", "content":result.content})
                .to_string()
                .len(),
        result.content,
    )
}

#[tokio::test]
async fn ten_listing_tasks_measured_bytes() {
    let artifacts = p1_module_tests::fixture_dir()
        .parent()
        .unwrap()
        .join("p1-module-ls");
    let bytes = std::fs::read(artifacts.join("p1-module-ls.wasm")).expect("build modules first");
    let mut release = Release::empty();
    release.add(json!({"name":"p1/ls", "digest":Digest::of(&bytes).to_string(), "path":"packages/ls/ls.wasm", "kind":"tool", "world":"p1:module/tool@1.0.0", "protocol":"1.0", "capabilities":["workspace","directory-listing"], "variant":"claude"}), &bytes);
    let loaded = release.loader().load("p1/ls").unwrap();
    let root = tempfile::tempdir().unwrap();
    for dir in [
        "small", "hidden", "empty", "links", "long", "logs", "tree/sub", "deep/a/b", "unicode",
        "paged",
    ] {
        std::fs::create_dir_all(root.path().join(dir)).unwrap();
    }
    for name in [
        "small/a",
        "small/b",
        "hidden/.dot",
        "hidden/visible",
        "links/file",
        "long/a",
        "long/b",
        "logs/a.log",
        "logs/b.rs",
        "tree/sub/child",
        "tree/root",
        "deep/a/b/leaf",
        "unicode/é",
        "unicode/文",
    ] {
        std::fs::write(root.path().join(name), b"fixture").unwrap();
    }
    std::os::unix::fs::symlink("file", root.path().join("links/link")).unwrap();
    for i in 0..600 {
        std::fs::File::create(root.path().join(format!("paged/f{i:03}"))).unwrap();
    }
    let workspace = Workspace::new(root.path())
        .unwrap()
        .with_credential_paths(Vec::new());
    let services = p1_host::catalog::capability_services_for(
        "p1/ls",
        workspace.clone(),
        ObservedFiles::new(),
        None,
    );
    let ls = wasm_tool(
        &loaded,
        services,
        ExecutionLimits::default(),
        &Arc::new(MaskCounter::new()),
    )
    .unwrap();
    let shell = ShellTool::new(workspace).with_env_snapshot(vec![
        ("LC_ALL".into(), "C".into()),
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), root.path().as_os_str().to_owned()),
    ]);
    let ls_declaration = serde_json::to_vec(ls.declaration()).unwrap().len();
    let shell_declaration = serde_json::to_vec(shell.declaration()).unwrap().len();
    let tasks = [
        (
            "flat",
            json!({"path":"small"}),
            "LC_ALL=C ls -1A -F -- small",
        ),
        (
            "hidden",
            json!({"path":"hidden"}),
            "LC_ALL=C ls -1A -F -- hidden",
        ),
        (
            "empty",
            json!({"path":"empty"}),
            "LC_ALL=C ls -1A -F -- empty",
        ),
        (
            "symlinks",
            json!({"path":"links"}),
            "LC_ALL=C ls -1A -F -- links",
        ),
        (
            "sizes",
            json!({"path":"long","long":true}),
            "LC_ALL=C ls -lA -- long",
        ),
        (
            "ignore",
            json!({"path":"logs","ignore":"*.log"}),
            "LC_ALL=C ls -1A -F --ignore='*.log' -- logs",
        ),
        (
            "tree",
            json!({"path":"tree","depth":2}),
            "find tree -mindepth 1 -maxdepth 2 -printf '%P%y\\n' | LC_ALL=C sort",
        ),
        (
            "deep",
            json!({"path":"deep","depth":4}),
            "find deep -mindepth 1 -maxdepth 4 -printf '%P%y\\n' | LC_ALL=C sort",
        ),
        (
            "unicode",
            json!({"path":"unicode"}),
            "LC_ALL=C ls -1A -F -- unicode",
        ),
        (
            "paged",
            json!({"path":"paged"}),
            "find paged -mindepth 1 -maxdepth 1 -printf '%f\\n' | LC_ALL=C sort | head -n 500",
        ),
    ];
    let mut total_ls = 0;
    let mut total_shell = 0;
    println!("BYTE_MEASUREMENT declarations ls={ls_declaration} shell={shell_declaration}");
    for (name, args, command) in tasks {
        let (mut ls_io, output) = run(ls.as_ref(), args.clone()).await;
        let (mut shell_io, _) = run(&shell, json!({"command":command})).await;
        let followups = if name == "paged" {
            let cursor = output
                .lines()
                .find_map(|l| l.strip_prefix("next cursor: "))
                .expect("next cursor");
            let mut next = args;
            next["cursor"] = json!(cursor);
            ls_io += run(ls.as_ref(), next).await.0;
            shell_io += run(&shell, json!({"command":"find paged -mindepth 1 -maxdepth 1 -printf '%f\\n' | LC_ALL=C sort | tail -n +501"})).await.0;
            1
        } else {
            0
        };
        // `shell` ships in every environment, so its declaration is in both arms;
        // shipping `ls` adds its own declaration to every request.
        let ls_total = ls_declaration + ls_io;
        let shell_total = shell_io;
        total_ls += ls_total;
        total_shell += shell_total;
        println!(
            "BYTE_MEASUREMENT {name} ls_io={ls_io} shell_io={shell_io} ls_total={ls_total} shell_total={shell_total} followups={followups}"
        );
    }
    println!(
        "BYTE_MEASUREMENT TOTAL ls={total_ls} shell={total_shell} ship={}",
        total_ls < total_shell
    );
}
