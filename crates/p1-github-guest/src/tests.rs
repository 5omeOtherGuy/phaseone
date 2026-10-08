use super::*;
use std::collections::VecDeque;

struct Scripted(VecDeque<(String, bool, Result<String, Error>)>);
impl Api for Scripted {
    fn get(&mut self, path: &str, raw: bool) -> Result<String, Error> {
        let (expected, expected_raw, result) = self.0.pop_front().expect("unexpected API request");
        assert_eq!(path, expected);
        assert_eq!(raw, expected_raw);
        result
    }
}
fn api(steps: Vec<(&str, bool, Value)>) -> Scripted {
    Scripted(
        steps
            .into_iter()
            .map(|(p, r, v)| (p.into(), r, Ok(v.to_string())))
            .collect(),
    )
}
fn run(name: &str, input: Value, api: &mut Scripted) -> Value {
    let result = execute(name, &input.to_string(), api).unwrap();
    assert!(api.0.is_empty());
    serde_json::from_str(&result).unwrap()
}
#[test]
fn reads_inclusive_numbered_ranges_and_applies_cap_after_slicing() {
    let content = format!("{}\nβeta\ngamma\nlast", "x".repeat(OUTPUT_BYTES + 1));
    let mut api = Scripted(VecDeque::from([
        (
            "/repos/o/r/contents/a%20b?ref=feature%2Fx".into(),
            false,
            Ok(json!({"type":"file"}).to_string()),
        ),
        (
            "/repos/o/r/contents/a%20b?ref=feature%2Fx".into(),
            true,
            Ok(content),
        ),
    ]));
    let result = execute(
        "read_github",
        &json!({"repository":"o/r","path":"a b","revision":"feature/x","read_range":[2,3]})
            .to_string(),
        &mut api,
    )
    .unwrap();
    assert_eq!(result, "2: βeta\n3: gamma\n");
    assert!(api.0.is_empty());
}
#[test]
fn directory_pages_sort_before_slicing_and_preserve_continuation() {
    let mut api = api(vec![(
        "/repos/o/r/contents/",
        false,
        json!([
            {"name":"z","type":"file"},{"name":"b","type":"dir"},{"name":"a","type":"file"},{"name":"c","type":"dir"}
        ]),
    )]);
    let result = run(
        "list_directory_github",
        json!({"repository":"o/r","limit":2,"offset":1}),
        &mut api,
    );
    assert_eq!(result["entries"], json!(["c/", "a"]));
    assert_eq!(result["next_offset"], 3);
    assert_eq!(result["entries_may_be_truncated"], false);
    let capped = json!(vec![json!({"name":"a","type":"file"}); 1000]);
    let capped: Value = serde_json::from_str(&directory_result(&capped, 1, 999).unwrap()).unwrap();
    assert_eq!(capped["entries_may_be_truncated"], true);
}
#[test]
fn glob_respects_segments_alternatives_classes_and_truncation() {
    let pattern = glob::compile("src/**/[ab]?.{rs,toml}").unwrap();
    assert!(pattern.is_match("src/a1.rs"));
    assert!(pattern.is_match("src/deep/b2.toml"));
    assert!(!pattern.is_match("src/c1.rs"));
    assert!(!pattern.is_match("src/a/1.rs"));
    assert!(!glob::compile("a[!x]b").unwrap().is_match("a/b"));
    for invalid in ["[", "{rs,toml", "{a,{b,c}}", "[]", "[a&&b]"] {
        assert!(glob::compile(invalid).is_err());
    }
    let mut api = api(vec![(
        "/repos/o/r/git/trees/tag?recursive=1",
        false,
        json!({"truncated":true,"tree":[]}),
    )]);
    assert!(
        execute(
            "glob_github",
            r#"{"repository":"o/r","revision":"tag","filePattern":"**/*.rs"}"#,
            &mut api
        )
        .is_err()
    );
}
#[test]
fn glob_default_revision_sorts_only_matching_blobs() {
    let mut api = api(vec![
        ("/repos/o/r", false, json!({"default_branch":"main"})),
        (
            "/repos/o/r/git/trees/main?recursive=1",
            false,
            json!({"truncated":false,"tree":[{"type":"tree","path":"dir.rs"},{"type":"blob","path":"z.rs"},{"type":"blob","path":"a.rs"},{"type":"blob","path":"b.txt"}]}),
        ),
    ]);
    let result = run(
        "glob_github",
        json!({"repository":"o/r","filePattern":"**/*.rs","limit":1,"offset":1}),
        &mut api,
    );
    assert_eq!(result["paths"], json!(["z.rs"]));
    assert_eq!(result["total"], 2);
    assert!(result["next_offset"].is_null());
}
#[test]
fn code_search_spans_pages_at_arbitrary_offset_and_reports_incomplete() {
    let first: Vec<_> = (0..100)
        .map(|n| json!({"path":format!("{n}.rs")}))
        .collect();
    let mut api = api(vec![
        (
            "/search/code?q=hello%20repo%3Ao%2Fr&per_page=100&page=1",
            false,
            json!({"items":first,"total_count":102,"incomplete_results":false}),
        ),
        (
            "/search/code?q=hello%20repo%3Ao%2Fr&per_page=100&page=2",
            false,
            json!({"items":[{"path":"100.rs","text_matches":[{"fragment":"context"}]},{"path":"101.rs"}],"total_count":102,"incomplete_results":true}),
        ),
    ]);
    let result = run(
        "search_github",
        json!({"repository":"o/r","pattern":"hello","offset":99,"limit":2}),
        &mut api,
    );
    assert_eq!(result["items"][0]["path"], "99.rs");
    assert_eq!(result["items"][1]["fragments"], json!(["context"]));
    assert_eq!(result["next_offset"], 101);
    assert_eq!(result["incomplete"], true);
}
#[test]
fn empty_incomplete_search_page_does_not_offer_a_stalled_continuation() {
    let mut api = api(vec![(
        "/search/code?q=hello%20repo%3Ao%2Fr&per_page=100&page=1",
        false,
        json!({"items":[],"total_count":12,"incomplete_results":true}),
    )]);
    let result = run(
        "search_github",
        json!({"repository":"o/r","pattern":"hello"}),
        &mut api,
    );
    assert!(result["next_offset"].is_null());
    assert_eq!(result["incomplete"], true);
}
#[test]
fn path_commit_scan_cap_returns_examined_offset_and_resume_reaches_match() {
    let rows = json!(vec![json!({"commit":{"message":"unrelated"}}); 100]);
    let mut api = Scripted(
        (1..=20)
            .map(|page| {
                (
                    format!("/repos/o/r/commits?per_page=100&page={page}&path=src%2Flib.rs"),
                    false,
                    Ok(rows.to_string()),
                )
            })
            .collect(),
    );
    let result = run(
        "commit_search",
        json!({"repository":"o/r","path":"src/lib.rs","query":"fix","offset":99,"limit":1}),
        &mut api,
    );
    assert_eq!(result["scanned"], 1901);
    assert_eq!(result["next_offset"], 2000);
    assert_eq!(result["incomplete"], true);
    api.0.push_back((
        "/repos/o/r/commits?per_page=100&page=21&path=src%2Flib.rs".into(),
        false,
        Ok(json!([{"sha":"hit","commit":{"message":"fix"}}]).to_string()),
    ));
    let resumed = run(
        "commit_search",
        json!({"repository":"o/r","path":"src/lib.rs","query":"fix","offset":2000,"limit":1}),
        &mut api,
    );
    assert_eq!(resumed["items"][0]["sha"], "hit");
    assert_eq!(resumed["scanned"], 1);
    assert!(resumed["next_offset"].is_null());
}
#[test]
fn path_commit_query_does_not_stop_at_the_first_nonmatching_page() {
    let first: Vec<_> = (0..100)
        .map(|n| json!({"sha":n.to_string(),"commit":{"message":"unrelated"}}))
        .collect();
    let mut api = api(vec![
        (
            "/repos/o/r/commits?per_page=100&page=1&path=src%2Flib.rs",
            false,
            json!(first),
        ),
        (
            "/repos/o/r/commits?per_page=100&page=2&path=src%2Flib.rs",
            false,
            json!([{"sha":"hit","commit":{"message":"FIX boundary\nDetails"}}]),
        ),
    ]);
    let result = run(
        "commit_search",
        json!({"repository":"o/r","path":"src/lib.rs","query":"fix boundary","limit":2}),
        &mut api,
    );
    assert_eq!(result["items"][0]["sha"], "hit");
    assert_eq!(result["scanned"], 101);
    assert_eq!(result["incomplete"], false);
    assert!(result["next_offset"].is_null());
}
#[test]
fn commit_query_uses_search_and_encodes_filters() {
    let endpoint = "/search/commits?q=fix%20repo%3Ao%2Fr%20author%3A%22alice%22%20committer-date%3A%22%3E%3D2024-01-01%22&per_page=100&page=1";
    let mut api = api(vec![(
        endpoint,
        false,
        json!({"items":[{"sha":"abc","commit":{"message":"fix\nreason"}}],"total_count":1,"incomplete_results":false}),
    )]);
    let result = run(
        "commit_search",
        json!({"repository":"o/r","query":"fix","author":"alice","since":"2024-01-01"}),
        &mut api,
    );
    assert_eq!(result["items"][0]["message"], "fix\nreason");
}
#[test]
fn diff_filters_exact_file_and_bounds_unicode_patches() {
    let mut api = api(vec![(
        "/repos/o/r/compare/main...feature%2Fx?per_page=1",
        false,
        json!({"status":"ahead","total_commits":3,"files":[{"filename":"src/a.rs","patch":"λ".repeat(4100),"additions":2,"deletions":1},{"filename":"src/a.rs.more","additions":10}]}),
    )]);
    let result = run(
        "diff_github",
        json!({"repository":"o/r","base":"main","head":"feature/x","path":"src/a.rs","includePatches":true}),
        &mut api,
    );
    assert_eq!(result["files"].as_array().unwrap().len(), 1);
    assert_eq!(result["files"][0]["additions"], 2);
    assert!(
        result["files"][0]["patch"]
            .as_str()
            .unwrap()
            .ends_with("[truncated]")
    );
}
#[test]
fn filtered_repository_discovery_keeps_private_repositories_and_offsets_matches() {
    let mut api = api(vec![(
        "/user/repos?sort=full_name&direction=asc&per_page=100&page=1",
        false,
        json!([
            {"name":"not-it","full_name":"me/not-it","owner":{"login":"me"},"language":"Rust"},
            {"name":"app-a","full_name":"me/app-a","owner":{"login":"me"},"language":"Rust","private":true},
            {"name":"app-b","full_name":"me/app-b","owner":{"login":"me"},"language":"rust","private":true}
        ]),
    )]);
    let result = run(
        "list_repositories",
        json!({"pattern":"app","organization":"ME","language":"rust","offset":1,"limit":1}),
        &mut api,
    );
    assert_eq!(result["items"][0]["repository"], "me/app-b");
    assert_eq!(result["items"][0]["private"], true);
    assert!(result["next_offset"].is_null());
}
#[test]
fn accessible_repositories_span_pages_without_mixing_public_results() {
    let first: Vec<_> = (0..100)
        .map(|n| json!({"full_name":format!("o/{n}")}))
        .collect();
    let mut api = api(vec![
        (
            "/user/repos?sort=full_name&direction=asc&per_page=100&page=1",
            false,
            json!(first),
        ),
        (
            "/user/repos?sort=full_name&direction=asc&per_page=100&page=2",
            false,
            json!([{"full_name":"o/100","private":true},{"full_name":"o/101"}]),
        ),
    ]);
    let result = run(
        "list_repositories",
        json!({"offset":99,"limit":2}),
        &mut api,
    );
    assert_eq!(result["items"][0]["repository"], "o/99");
    assert_eq!(result["items"][1]["repository"], "o/100");
    assert_eq!(result["next_offset"], 101);
    assert_eq!(result["source"], "accessible");
}
#[test]
fn anonymous_repository_search_uses_only_the_public_source() {
    let mut api = Scripted(VecDeque::from([
        ("/user/repos?sort=full_name&direction=asc&per_page=100&page=1".into(), false, Err(Error::Message("GitHub authentication required".into()))),
        ("/search/repositories?q=app%20in%3Aname&per_page=100&page=1".into(), false, Ok(json!({"items":[{"full_name":"public/app"}],"total_count":1,"incomplete_results":false}).to_string())),
    ]));
    let result = run("list_repositories", json!({"pattern":"app"}), &mut api);
    assert_eq!(result["source"], "search");
    assert_eq!(result["items"][0]["repository"], "public/app");
}
#[test]
fn invalid_arguments_never_dispatch_and_cancellation_is_preserved() {
    for (tool, input) in [
        (
            "read_github",
            json!({"repository":"o/r","path":"a","read_range":[3,2]}),
        ),
        ("read_github", json!({"repository":"o/r","path":"../a"})),
        (
            "glob_github",
            json!({"repository":"o/r","filePattern":"*","limit":0}),
        ),
        (
            "glob_github",
            json!({"repository":"o/r","filePattern":"*","limit":null}),
        ),
        (
            "list_directory_github",
            json!({"repository":"o/r","limit":1.5}),
        ),
        ("list_repositories", json!({"offset":1_000_001})),
        (
            "search_github",
            json!({"repository":"o/r","pattern":"hello repo:other/repo"}),
        ),
        (
            "diff_github",
            json!({"repository":"o/r","base":"main","head":"tag","token":"not-accepted"}),
        ),
    ] {
        assert!(execute(tool, &input.to_string(), &mut api(vec![])).is_err());
    }
    for invalid in [
        "https://example.com/o/r",
        "https://github.com/o/r/tree/main",
        "search/code",
        "o/..",
    ] {
        assert!(repository(invalid).is_err());
    }
    assert_eq!(repository("https://github.com/o/r.git/").unwrap(), "o/r");
    let mut api = Scripted(VecDeque::from([(
        "/repos/o/r/contents/a".into(),
        false,
        Err(Error::Cancelled),
    )]));
    assert_eq!(
        execute(
            "read_github",
            r#"{"repository":"o/r","path":"a"}"#,
            &mut api
        ),
        Err(Error::Cancelled)
    );
}
