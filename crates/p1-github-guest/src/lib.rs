//! GitHub-only research tools, adapted from ampi's src/extensions/ampi-github/.
//! No networking, credentials, filesystem or shell: the host supplies bounded GETs.
#![forbid(unsafe_code)]

mod glob;
mod schema;
#[cfg(test)]
mod tests;

pub use schema::{TOOLS, description, input_schema};
use serde_json::{Value, json};

const OUTPUT_BYTES: usize = 131_072;
const MAX_SCAN_PAGES: usize = 20;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Cancelled,
    Message(String),
}
impl From<String> for Error {
    fn from(s: String) -> Self {
        Self::Message(s)
    }
}
impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Self::Message(s.into())
    }
}

/// The only authority the guest needs. `path` is an encoded API path and query,
/// not a URL. `raw` requests file bytes instead of the JSON contents envelope.
pub trait Api {
    fn get(&mut self, path: &str, raw: bool) -> Result<String, Error>;
}

pub fn execute(tool: &str, raw: &str, api: &mut impl Api) -> Result<String, Error> {
    let input: Value = serde_json::from_str(raw).map_err(|_| "invalid JSON arguments")?;
    schema::validate(tool, &input)?;
    let repo = if tool == "list_repositories" {
        String::new()
    } else {
        repository(string(&input, "repository")?)?
    };
    let base = format!("/repos/{repo}");
    let result = match tool {
        "read_github" => read(&base, &input, api)?,
        "list_directory_github" => directory(&base, &input, api)?,
        "glob_github" => glob_files(&base, &input, api)?,
        "search_github" => search(&repo, &input, api)?,
        "commit_search" => commits(&base, &repo, &input, api)?,
        "diff_github" => diff(&base, &input, api)?,
        "list_repositories" => repositories(&input, api)?,
        _ => return Err("unknown GitHub tool".into()),
    };
    if result.len() > OUTPUT_BYTES {
        return Err(
            "result exceeds 128 KiB; narrow the query, reduce limit, or use read_range".into(),
        );
    }
    Ok(result)
}

fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str, Error> {
    v[key]
        .as_str()
        .ok_or_else(|| format!("{key} must be a string").into())
}
fn optional<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v[key].as_str()
}
fn number(v: &Value, key: &str, default: usize) -> usize {
    v[key].as_u64().map(|n| n as usize).unwrap_or(default)
}
fn json_get(api: &mut impl Api, path: &str) -> Result<Value, Error> {
    serde_json::from_str(&api.get(path, false)?)
        .map_err(|_| "GitHub returned malformed JSON".into())
}
fn array(v: &Value) -> Result<&Vec<Value>, Error> {
    v.as_array()
        .ok_or_else(|| "GitHub returned an unexpected result shape".into())
}
fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_owned()
}

/// URL syntax is deliberately narrower than ampi: tree/blob/page URLs are refused
/// rather than silently discarding their revision and path.
pub fn repository(value: &str) -> Result<String, Error> {
    let value = value.strip_prefix("https://github.com/").unwrap_or(value);
    let value = value
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or(value.trim_end_matches('/'));
    let parts: Vec<_> = value.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|s| {
            s.is_empty()
                || *s == "."
                || *s == ".."
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
        || [
            "search",
            "settings",
            "orgs",
            "users",
            "topics",
            "collections",
            "marketplace",
            "login",
        ]
        .contains(&parts[0])
    {
        return Err(
            "repository must be owner/repo or https://github.com/owner/repo, not a page URL".into(),
        );
    }
    Ok(value.to_owned())
}

fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            write!(out, "%{b:02X}").expect("write to string");
        }
    }
    out
}
fn path(value: &str) -> Result<String, Error> {
    let value = value.trim_matches('/');
    if value.split('/').any(|s| s == "." || s == "..")
        || value.contains('\\')
        || value.chars().any(char::is_control)
    {
        return Err("path must be relative without dot segments or control characters".into());
    }
    Ok(value.split('/').map(encode).collect::<Vec<_>>().join("/"))
}
fn query(endpoint: &str, args: &[(&str, String)]) -> String {
    let args = args
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    if args.is_empty() {
        endpoint.into()
    } else {
        format!("{endpoint}?{args}")
    }
}
fn contents_endpoint(base: &str, input: &Value) -> Result<String, Error> {
    let endpoint = format!(
        "{base}/contents/{}",
        path(optional(input, "path").unwrap_or_default())?
    );
    let args = optional(input, "revision")
        .map(|r| vec![("ref", r.into())])
        .unwrap_or_default();
    Ok(query(&endpoint, &args))
}

fn read(base: &str, input: &Value, api: &mut impl Api) -> Result<String, Error> {
    let endpoint = contents_endpoint(base, input)?;
    let metadata = json_get(api, &endpoint)?;
    if metadata.is_array() {
        return directory_result(&metadata, 100, 0);
    }
    if metadata["type"] != "file" {
        return Err("path is not a regular file or directory".into());
    }
    let contents = api.get(&endpoint, true)?;
    if contents.contains('\0') {
        return Err("file is binary; only UTF-8 text is supported".into());
    }
    let range = input["read_range"].as_array();
    let start = range.map(|r| r[0].as_u64().unwrap() as usize).unwrap_or(1);
    let end = range
        .map(|r| r[1].as_u64().unwrap() as usize)
        .unwrap_or(usize::MAX);
    let mut output = String::new();
    for (index, line) in contents
        .lines()
        .enumerate()
        .skip(start - 1)
        .take(end - start + 1)
    {
        use std::fmt::Write;
        writeln!(output, "{}: {line}", index + 1).expect("write to string");
        if output.len() > OUTPUT_BYTES {
            return Err("file slice exceeds 128 KiB; use a smaller read_range".into());
        }
    }
    Ok(output)
}
fn directory(base: &str, input: &Value, api: &mut impl Api) -> Result<String, Error> {
    let data = json_get(api, &contents_endpoint(base, input)?)?;
    directory_result(
        &data,
        number(input, "limit", 100),
        number(input, "offset", 0),
    )
}
fn directory_result(data: &Value, limit: usize, offset: usize) -> Result<String, Error> {
    let mut entries = array(data)?.clone();
    entries.sort_by_key(|e| (e["type"] != "dir", text(e, "name")));
    let items: Vec<_> = entries
        .iter()
        .skip(offset)
        .take(limit)
        .map(|e| {
            format!(
                "{}{}",
                text(e, "name"),
                if e["type"] == "dir" { "/" } else { "" }
            )
        })
        .collect();
    Ok(json!({"entries":items,"total":entries.len(),"next_offset":next(offset, items.len(), entries.len()),"entries_may_be_truncated":entries.len()>=1000}).to_string())
}
fn next(offset: usize, returned: usize, total: usize) -> Option<usize> {
    let end = offset.saturating_add(returned);
    (end < total).then_some(end)
}
fn glob_files(base: &str, input: &Value, api: &mut impl Api) -> Result<String, Error> {
    let pattern = glob::compile(string(input, "filePattern")?)?;
    let revision = match optional(input, "revision") {
        Some(r) => r.to_owned(),
        None => string(&json_get(api, base)?, "default_branch")?.to_owned(),
    };
    let tree = json_get(
        api,
        &format!("{base}/git/trees/{}?recursive=1", encode(&revision)),
    )?;
    if tree["truncated"] != false {
        return Err("GitHub tree is truncated or lacks completeness metadata; narrow the revision/repository".into());
    }
    let mut paths: Vec<_> = array(&tree["tree"])?
        .iter()
        .filter(|e| e["type"] == "blob")
        .filter_map(|e| e["path"].as_str())
        .filter(|s| pattern.is_match(s))
        .collect();
    paths.sort_unstable();
    let offset = number(input, "offset", 0);
    let page: Vec<_> = paths
        .iter()
        .skip(offset)
        .take(number(input, "limit", 100))
        .collect();
    Ok(
        json!({"paths":page,"total":paths.len(),"next_offset":next(offset,page.len(),paths.len())})
            .to_string(),
    )
}

// Users supply search terms, not repository selectors that can widen our one-repo scope.
fn scoped_query(value: &str) -> Result<&str, Error> {
    if value.trim().is_empty()
        || value.split_whitespace().any(|s| {
            let s = s.trim_start_matches(['-', '(', '"']).to_ascii_lowercase();
            ["repo:", "org:", "user:"]
                .iter()
                .any(|prefix| s.starts_with(prefix))
        })
    {
        return Err(
            "query needs search terms and must not contain repo:, org: or user: selectors".into(),
        );
    }
    Ok(value)
}
fn qualifier(name: &str, value: &str) -> Result<String, Error> {
    if value.contains(['"', '\\']) || value.chars().any(char::is_control) {
        return Err(format!("{name} contains unsupported query characters").into());
    }
    Ok(format!("{name}:\"{value}\""))
}

/// REST pagination always fetches 100 items, then slices the requested offset.
/// Arbitrary offsets work, including a page spanning a REST page boundary.
fn search_page(
    api: &mut impl Api,
    endpoint: &str,
    q: String,
    limit: usize,
    offset: usize,
) -> Result<Value, Error> {
    if offset >= 1000 {
        return Err("GitHub search exposes at most 1000 results; narrow the query".into());
    }
    let mut items = Vec::new();
    let mut total = 0;
    let mut incomplete = false;
    let mut exhausted = false;
    for page in offset / 100 + 1..=10 {
        let data = json_get(
            api,
            &query(
                endpoint,
                &[
                    ("q", q.clone()),
                    ("per_page", "100".into()),
                    ("page", page.to_string()),
                ],
            ),
        )?;
        total = data["total_count"].as_u64().ok_or("missing search total")? as usize;
        incomplete |= data["incomplete_results"].as_bool().unwrap_or(true);
        let rows = array(&data["items"])?;
        let skip = if page == offset / 100 + 1 {
            offset % 100
        } else {
            0
        };
        items.extend(rows.iter().skip(skip).take(limit - items.len()).cloned());
        exhausted = rows.len() < 100 && offset + items.len() >= (page - 1) * 100 + rows.len();
        incomplete |= exhausted && offset + items.len() < total;
        if items.len() == limit || rows.len() < 100 || page * 100 >= total {
            break;
        }
    }
    let next_offset = if exhausted {
        None
    } else {
        next(offset, items.len(), total.min(1000))
    };
    Ok(
        json!({"items":items,"total":total,"next_offset":next_offset,"incomplete":incomplete || total > 1000}),
    )
}
fn search(repo: &str, input: &Value, api: &mut impl Api) -> Result<String, Error> {
    let mut q = format!("{} repo:{repo}", scoped_query(string(input, "pattern")?)?);
    if let Some(p) = optional(input, "path") {
        q.push(' ');
        q.push_str(&qualifier("path", p)?);
    }
    let mut data = search_page(
        api,
        "/search/code",
        q,
        number(input, "limit", 30),
        number(input, "offset", 0),
    )?;
    for item in data["items"].as_array_mut().unwrap() {
        let fragments: Vec<_> = item["text_matches"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["fragment"].as_str())
            .map(|s| clip(s, 2048))
            .collect();
        *item = json!({"path":item["path"],"url":item["html_url"],"fragments":fragments});
    }
    Ok(data.to_string())
}
fn clip(value: &str, max: usize) -> String {
    let mut chars = value.chars();
    let mut out: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        out.push_str("\n[truncated]");
    }
    out
}
fn commit_item(item: &Value) -> Value {
    json!({"sha":item["sha"],"url":item["html_url"],
        "message":clip(item["commit"]["message"].as_str().unwrap_or_default(),1024),
        "author":item["commit"]["author"]["name"],"date":item["commit"]["author"]["date"]})
}
fn commits(base: &str, repo: &str, input: &Value, api: &mut impl Api) -> Result<String, Error> {
    let limit = number(input, "limit", 50);
    let offset = number(input, "offset", 0);
    if let Some(q) = optional(input, "query") {
        scoped_query(q)?;
        if optional(input, "path").is_none() {
            let mut q = format!("{q} repo:{repo}");
            for (key, qualifier_key) in [
                ("author", "author"),
                ("since", "committer-date"),
                ("until", "committer-date"),
            ] {
                if let Some(s) = optional(input, key) {
                    let prefix = match key {
                        "since" => ">=",
                        "until" => "<=",
                        _ => "",
                    };
                    q.push_str(&format!(
                        " {}",
                        qualifier(qualifier_key, &format!("{prefix}{s}"))?
                    ));
                }
            }
            let mut data = search_page(api, "/search/commits", q, limit, offset)?;
            for item in data["items"].as_array_mut().unwrap() {
                *item = commit_item(item);
            }
            return Ok(data.to_string());
        }
    }
    // Query + path is a local message substring search over path-filtered commits.
    // Scan successive pages and return an explicit continuation if our scan cap is hit.
    let mut items = Vec::new();
    let mut scanned = offset;
    let mut exhausted = false;
    for page in offset / 100 + 1..offset / 100 + 1 + MAX_SCAN_PAGES {
        let mut args = vec![("per_page", "100".into()), ("page", page.to_string())];
        for key in ["path", "author", "since", "until"] {
            if let Some(s) = optional(input, key) {
                args.push((key, s.into()));
            }
        }
        let rows = json_get(api, &query(&format!("{base}/commits"), &args))?;
        let rows = array(&rows)?;
        let skip = if page == offset / 100 + 1 {
            offset % 100
        } else {
            0
        };
        for row in rows.iter().skip(skip) {
            scanned += 1;
            let matches = optional(input, "query").is_none_or(|q| {
                text(&row["commit"], "message")
                    .to_lowercase()
                    .contains(&q.to_lowercase())
            });
            if matches {
                items.push(commit_item(row));
            }
            if items.len() == limit {
                break;
            }
        }
        exhausted = rows.len() < 100 && scanned >= (page - 1) * 100 + rows.len();
        if items.len() == limit || exhausted {
            break;
        }
    }
    Ok(json!({"items":items,"scanned":scanned-offset,"next_offset":if exhausted {None} else {Some(scanned)},
        "incomplete":!exhausted && items.len()<limit,"query_mode":if optional(input,"query").is_some(){"message_substring"}else{"list"}}).to_string())
}
fn diff(base: &str, input: &Value, api: &mut impl Api) -> Result<String, Error> {
    let endpoint = format!(
        "{base}/compare/{}...{}",
        encode(string(input, "base")?),
        encode(string(input, "head")?)
    );
    let data = json_get(api, &format!("{endpoint}?per_page=1"))?;
    let rows = array(&data["files"])?;
    let files: Vec<_> = rows.iter().filter(|r| optional(input,"path").is_none_or(|p| r["filename"]==p)).map(|r| {
        let mut file = json!({"path":r["filename"],"previous_path":r["previous_filename"],"status":r["status"],"additions":r["additions"],"deletions":r["deletions"]});
        if input["includePatches"] == true {
            file["patch"] = Value::String(r["patch"].as_str().map(|s| clip(s,4096)).unwrap_or_else(|| "[patch unavailable: binary or large file]".into()));
        }
        file
    }).collect();
    Ok(json!({"files":files,"status":data["status"],"total_commits":data["total_commits"],"files_may_be_truncated":rows.len()>=300}).to_string())
}
fn repo_item(row: &Value) -> Value {
    json!({"repository":row["full_name"],"url":row["html_url"],"description":row["description"],
        "language":row["language"],"private":row["private"],"stars":row["stargazers_count"],"fork":row["fork"],"archived":row["archived"]})
}
fn repositories(input: &Value, api: &mut impl Api) -> Result<String, Error> {
    let filtered = ["pattern", "organization", "language"]
        .iter()
        .any(|key| optional(input, key).is_some());
    if filtered {
        let mut matches = Vec::new();
        let mut complete = false;
        for page in 1..=MAX_SCAN_PAGES {
            let result = json_get(
                api,
                &format!("/user/repos?sort=full_name&direction=asc&per_page=100&page={page}"),
            );
            let rows = match result {
                Err(Error::Message(s)) if s == "GitHub authentication required" => {
                    complete = true;
                    break;
                }
                result => result?,
            };
            let rows = array(&rows)?;
            matches.extend(
                rows.iter()
                    .filter(|r| {
                        optional(input, "pattern").is_none_or(|p| {
                            text(r, "name").to_lowercase().contains(&p.to_lowercase())
                        }) && optional(input, "organization")
                            .is_none_or(|p| text(&r["owner"], "login").eq_ignore_ascii_case(p))
                            && optional(input, "language")
                                .is_none_or(|p| text(r, "language").eq_ignore_ascii_case(p))
                    })
                    .map(repo_item),
            );
            if rows.len() < 100 {
                complete = true;
                break;
            }
        }
        if !matches.is_empty() || !complete {
            let offset = number(input, "offset", 0);
            let items: Vec<_> = matches
                .iter()
                .skip(offset)
                .take(number(input, "limit", 30))
                .collect();
            return Ok(json!({"items":items,"source":"accessible","known_matches":matches.len(),"incomplete":!complete,"next_offset":next(offset,items.len(),matches.len())}).to_string());
        }
    }
    // No filters: stable, accessible-repository enumeration, never a mixture of
    // unrelated search pages. Anonymous callers fall back on public search only.
    if !filtered {
        let offset = number(input, "offset", 0);
        let limit = number(input, "limit", 30);
        let mut items = Vec::new();
        let mut exhausted = false;
        for page in offset / 100 + 1..=offset / 100 + 2 {
            let result = json_get(
                api,
                &format!("/user/repos?sort=full_name&direction=asc&per_page=100&page={page}"),
            );
            let rows = match result {
                Err(Error::Message(s)) if s == "GitHub authentication required" => break,
                result => result?,
            };
            let rows = array(&rows)?;
            let skip = if page == offset / 100 + 1 {
                offset % 100
            } else {
                0
            };
            items.extend(
                rows.iter()
                    .skip(skip)
                    .take(limit - items.len())
                    .map(repo_item),
            );
            exhausted = rows.len() < 100 && offset + items.len() >= (page - 1) * 100 + rows.len();
            if items.len() == limit || rows.len() < 100 {
                break;
            }
        }
        if !items.is_empty() || exhausted {
            return Ok(json!({"items":items,"source":"accessible","next_offset":if exhausted {None}else{Some(offset+items.len())}}).to_string());
        }
    }
    let mut q = match optional(input, "pattern") {
        Some(p) => format!("{} in:name", scoped_query(p)?),
        None => "stars:>=0".into(),
    };
    for (key, qkey) in [("organization", "user"), ("language", "language")] {
        if let Some(s) = optional(input, key) {
            q.push_str(&format!(" {}", qualifier(qkey, s)?));
        }
    }
    let mut data = search_page(
        api,
        "/search/repositories",
        q,
        number(input, "limit", 30),
        number(input, "offset", 0),
    )?;
    for row in data["items"].as_array_mut().unwrap() {
        *row = repo_item(row);
    }
    data["source"] = "search".into();
    Ok(data.to_string())
}
