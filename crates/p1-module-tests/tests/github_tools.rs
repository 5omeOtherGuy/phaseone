//! Real components over a scripted host service: no network, shell or credentials.
use p1_contracts::serde_json::{Value, json};
use p1_contracts::{
    BoxFuture, CancellationToken, Effect, ToolCall, ToolContext, ToolInput, ToolStatus,
};
use p1_module_runtime::github::{GithubError, GithubService};
use p1_module_runtime::{ExecutionLimits, Loader, ReleaseManifest, Services, wasm_tool};
use p1_redact::MaskCounter;
use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
};

struct Scripted(Mutex<VecDeque<(String, bool, String)>>);
impl GithubService for Scripted {
    fn get(
        &self,
        path: String,
        raw: bool,
        _: CancellationToken,
    ) -> BoxFuture<'_, Result<String, GithubError>> {
        Box::pin(async move {
            let (expected, expected_raw, result) = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected request");
            assert_eq!(path, expected);
            assert_eq!(raw, expected_raw);
            Ok(result)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seven_components_declare_inspect_and_execute_independently() {
    let cases=[
        ("read-github","read_github",json!({"repository":"o/r","path":"README.md","read_range":[2,2]}),vec![("/repos/o/r/contents/README.md",false,json!({"type":"file"}).to_string()),("/repos/o/r/contents/README.md",true,"first\nsecond\nthird".into())],"2: second\n"),
        ("list-directory-github","list_directory_github",json!({"repository":"o/r"}),vec![("/repos/o/r/contents/",false,json!([{"name":"src","type":"dir"}]).to_string())],"src/"),
        ("glob-github","glob_github",json!({"repository":"o/r","revision":"main","filePattern":"**/*.rs"}),vec![("/repos/o/r/git/trees/main?recursive=1",false,json!({"truncated":false,"tree":[{"path":"lib.rs","type":"blob"}]}).to_string())],"lib.rs"),
        ("search-github","search_github",json!({"repository":"o/r","pattern":"needle"}),vec![("/search/code?q=needle%20repo%3Ao%2Fr&per_page=100&page=1",false,json!({"total_count":1,"incomplete_results":false,"items":[{"path":"found.rs","text_matches":[{"fragment":"needle"}]}]}).to_string())],"found.rs"),
        ("commit-search","commit_search",json!({"repository":"o/r"}),vec![("/repos/o/r/commits?per_page=100&page=1",false,json!([{"sha":"abcdef","commit":{"message":"why changed"}}]).to_string())],"why changed"),
        ("diff-github","diff_github",json!({"repository":"o/r","base":"main","head":"tag"}),vec![("/repos/o/r/compare/main...tag?per_page=1",false,json!({"files":[{"filename":"changed.rs","additions":3}],"total_commits":2,"status":"ahead"}).to_string())],"changed.rs"),
        ("list-repositories","list_repositories",json!({}),vec![("/user/repos?sort=full_name&direction=asc&per_page=100&page=1",false,json!([{"full_name":"o/private","private":true}]).to_string())],"o/private"),
    ];
    for (suffix, name, args, steps, expected) in cases {
        let package = format!("p1-module-{suffix}");
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../modules/target/p1-modules")
            .join(&package);
        let bytes =
            std::fs::read(dir.join(format!("{package}.wasm"))).expect("build GitHub modules first");
        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(format!("{package}.manifest.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["capabilities"], json!(["github-api"]));
        let release = tempfile::tempdir().unwrap();
        std::fs::write(release.path().join("tool.wasm"), &bytes).unwrap();
        let entry = json!({"name":manifest["name"],"digest":manifest["digest"],"path":"tool.wasm","world":manifest["world"],"kind":"tool","protocol":"1.0","capabilities":["github-api"],"variant":"github"});
        let path = release.path().join("manifest.json");
        std::fs::write(
            &path,
            json!({"format":"p1-release-manifest/1","components":[entry]}).to_string(),
        )
        .unwrap();
        let loader = Loader::new(ReleaseManifest::read(&path).unwrap(), release.path()).unwrap();
        let loaded = loader.load(&format!("p1/{suffix}")).unwrap();
        let mask = Arc::new(MaskCounter::new());
        assert!(
            wasm_tool(
                &loaded,
                Services::default(),
                ExecutionLimits::default(),
                &mask
            )
            .is_err(),
            "service must be required"
        );
        let api = Arc::new(Scripted(Mutex::new(
            steps
                .into_iter()
                .map(|(p, r, s)| (p.into(), r, s))
                .collect(),
        )));
        let tool = wasm_tool(
            &loaded,
            Services {
                github: Some(api.clone()),
                ..Services::default()
            },
            ExecutionLimits::default(),
            &mask,
        )
        .unwrap();
        assert_eq!(tool.declaration().name, name);
        let call = ToolCall {
            call_id: "c1".into(),
            name: name.into(),
            input: ToolInput::Json(args.to_string()),
        };
        let before = api.0.lock().unwrap().len();
        assert_eq!(tool.effect(&call), Effect::ReadOnly);
        assert_eq!(tool.describe(&call).verb, "search");
        assert_eq!(
            api.0.lock().unwrap().len(),
            before,
            "inspection must not call host"
        );
        let outcome = tool
            .execute(
                &call,
                ToolContext {
                    cancel: CancellationToken::new(),
                },
            )
            .await;
        assert_eq!(
            outcome.status,
            ToolStatus::Ok,
            "{name}: {}",
            outcome.content
        );
        assert!(
            outcome.content.contains(expected),
            "{name}: {}",
            outcome.content
        );
        assert!(api.0.lock().unwrap().is_empty());
        let invalid = ToolCall {
            input: ToolInput::Json("{\"unrecognized\":true}".into()),
            ..call
        };
        assert_eq!(
            tool.execute(
                &invalid,
                ToolContext {
                    cancel: CancellationToken::new()
                }
            )
            .await
            .status,
            ToolStatus::Error
        );
    }
}
