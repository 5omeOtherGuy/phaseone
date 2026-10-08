use crate::Error;
use serde_json::{Value, json};

pub const TOOLS: [&str; 7] = [
    "read_github",
    "list_directory_github",
    "glob_github",
    "search_github",
    "commit_search",
    "diff_github",
    "list_repositories",
];

pub fn description(tool: &str) -> &'static str {
    match tool {
        "read_github" => {
            "Read UTF-8 file contents with line numbers, or list a directory in one GitHub repository. Use read_range for large files. Read-only; credentials stay in the host."
        }
        "list_directory_github" => {
            "List a GitHub repository directory, directories first with trailing slashes. Defaults to root. Use next_offset to page results."
        }
        "glob_github" => {
            "Find repository files matching *, **, ?, {a,b}, or [...] glob syntax. Refuses truncated GitHub trees instead of claiming complete results."
        }
        "search_github" => {
            "Search code in one GitHub repository with surrounding text fragments. Requires a host GitHub token. REST search is default-branch only and exposes at most 1000 results."
        }
        "commit_search" => {
            "Search commit messages, or list commits filtered by path, author and date. Query plus path uses case-insensitive message-substring matching over successive path-filtered commit pages; next_offset resumes scanning. Read-only."
        }
        "diff_github" => {
            "Compare GitHub branches, tags or SHAs. Returns file stats; includePatches includes bounded unified patches. Path selects exactly one file. GitHub comparisons expose at most 300 changed files."
        }
        "list_repositories" => {
            "Discover GitHub repositories accessible to the host token, with public search fallback. Filter by name, owner or primary language. Filtered discovery scans the first 20 API pages; next_offset pages known matches only. An incomplete result is not exhaustive even when next_offset is null."
        }
        _ => "Unknown GitHub tool",
    }
}

pub fn input_schema(tool: &str) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    let (strings, optional_strings, default, max): (&[&str], &[&str], usize, usize) = match tool {
        "read_github" => (&["repository", "path"], &["revision"], 0, 0),
        "list_directory_github" => (&["repository"], &["path", "revision"], 100, 1000),
        "glob_github" => (&["repository", "filePattern"], &["revision"], 100, 300),
        "search_github" => (&["repository", "pattern"], &["path"], 30, 100),
        "commit_search" => (
            &["repository"],
            &["query", "path", "author", "since", "until"],
            50,
            100,
        ),
        "diff_github" => (&["repository", "base", "head"], &["path"], 0, 0),
        "list_repositories" => (&[], &["pattern", "organization", "language"], 30, 100),
        _ => (&[], &[], 0, 0),
    };
    for key in strings.iter().chain(optional_strings) {
        let min = if *key == "path" { 0 } else { 1 };
        let desc = match *key {
            "repository" => "One owner/repo or https://github.com/owner/repo; no page URLs.",
            "revision" => "Branch, tag or SHA; defaults to the repository default branch.",
            "filePattern" => "Repository-relative glob, e.g. src/**/*.{rs,toml}.",
            "since" => "ISO-8601 inclusive lower date bound.",
            "until" => "ISO-8601 inclusive upper date bound.",
            "organization" => "Organization or user login.",
            "query" => "Commit-message search terms; with path, a literal message substring.",
            _ => "Search term, repository path or ref as appropriate; never credentials.",
        };
        properties.insert(
            (*key).into(),
            json!({"type":"string","minLength":min,"maxLength":4096,"description":desc}),
        );
    }
    required.extend(strings.iter().copied());
    if max > 0 {
        properties.insert(
            "limit".into(),
            json!({"type":"integer","minimum":1,"maximum":max,"default":default}),
        );
        properties.insert("offset".into(),json!({"type":"integer","minimum":0,"maximum":1_000_000,"default":0,"description":"Resume at next_offset from a previous result; arbitrary offsets are supported. For path-filtered commit search this counts examined commits."}));
    }
    if tool == "read_github" {
        properties.insert("read_range".into(),json!({"type":"array","minItems":2,"maxItems":2,"items":{"type":"integer","minimum":1,"maximum":1_000_000},"description":"[start,end] line numbers, 1-based inclusive; end must be at least start."}));
    }
    if tool == "diff_github" {
        properties.insert(
            "includePatches".into(),
            json!({"type":"boolean","default":false}),
        );
    }
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

pub(crate) fn validate(tool: &str, input: &Value) -> Result<(), Error> {
    if !TOOLS.contains(&tool) {
        return Err("unknown GitHub tool".into());
    }
    let schema = input_schema(tool);
    let object = input.as_object().ok_or("arguments must be an object")?;
    for key in schema["required"].as_array().unwrap() {
        let key = key.as_str().unwrap();
        if !object.contains_key(key) {
            return Err(format!("missing required field {key}").into());
        }
    }
    for (key, value) in object {
        let property = &schema["properties"][key];
        let valid = match property["type"].as_str() {
            Some("string") => value.as_str().is_some_and(|s| {
                s.chars().count() >= property["minLength"].as_u64().unwrap() as usize
                    && s.len() <= 4096
                    && !s.chars().any(char::is_control)
            }),
            Some("integer") => value.as_u64().is_some_and(|n| {
                n >= property["minimum"].as_u64().unwrap()
                    && n <= property["maximum"].as_u64().unwrap()
            }),
            Some("boolean") => value.is_boolean(),
            Some("array") => value.as_array().is_some_and(|a| {
                a.len() == 2
                    && a.iter()
                        .all(|n| n.as_u64().is_some_and(|n| (1..=1_000_000).contains(&n)))
                    && a[1].as_u64() >= a[0].as_u64()
            }),
            _ => false,
        };
        if !valid {
            return Err(format!("invalid or unknown field {key}").into());
        }
    }
    Ok(())
}
