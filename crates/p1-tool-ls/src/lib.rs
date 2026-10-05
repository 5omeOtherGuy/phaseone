//! Pure guest computation for `p1/ls`; confinement and the walk belong to the host.
//! Adapted from /home/phaseonebig/projects/iris-agent/src/tools/ls.rs.
use serde::Deserialize;
use serde_json::{Value, json};

pub const NAME: &str = "ls";
pub const DESCRIPTION: &str = "List workspace directory entries, including hidden names. Symlinks are marked @ and never followed. Use depth for a tree, long for sizes, ignore for exclusion globs, and cursor to continue a bounded page.";

pub fn input_schema() -> Value {
    json!({"type":"object","properties":{
        "path":{"type":"string","default":"."},
        "limit":{"type":"integer","minimum":1,"maximum":500,"default":500},
        "depth":{"type":"integer","minimum":1,"default":1},
        "long":{"type":"boolean","default":false},
        "ignore":{"anyOf":[{"type":"string"},{"type":"array","items":{"type":"string"}}]},
        "cursor":{"type":"string"}
    },"additionalProperties":false})
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Ignore {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    #[serde(default = "default_path")]
    pub path: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default = "default_depth")]
    pub depth: u32,
    #[serde(default)]
    pub long: bool,
    #[serde(default)]
    pub ignore: Option<Ignore>,
    #[serde(default)]
    pub cursor: Option<String>,
}
fn default_path() -> String {
    ".".into()
}
fn default_limit() -> u32 {
    500
}
fn default_depth() -> u32 {
    1
}

impl Input {
    pub fn parse(json: &str) -> Result<Self, String> {
        let input: Self =
            serde_json::from_str(json).map_err(|e| format!("invalid ls input: {e}"))?;
        if input.depth == 0 || !(1..=500).contains(&input.limit) {
            return Err("invalid ls input: depth must be >= 1 and limit must be 1..500".into());
        }
        Ok(input)
    }
    pub fn patterns(&self) -> &[String] {
        match &self.ignore {
            None => &[],
            Some(Ignore::One(pattern)) => std::slice::from_ref(pattern),
            Some(Ignore::Many(patterns)) => patterns,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    Symlink,
    Other,
}
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub depth: u32,
}
pub struct Page {
    pub entries: Vec<Entry>,
    pub next: Option<String>,
    pub scanned: u64,
    pub scan_capped: bool,
}

pub fn render(page: &Page, long: bool) -> String {
    let mut body = String::new();
    for entry in &page.entries {
        if long {
            let marker = match entry.kind {
                Kind::File => "f",
                Kind::Directory => "d",
                Kind::Symlink => "l",
                Kind::Other => "o",
            };
            let size = if entry.kind == Kind::File {
                format!("{} B", entry.size)
            } else {
                "-".into()
            };
            body.push_str(&format!("{marker} {size:>8} "));
        }
        body.push_str(&"  ".repeat(entry.depth.saturating_sub(1) as usize));
        let name = entry.path.rsplit('/').next().unwrap_or(&entry.path);
        // Quoting control characters makes filenames unambiguous without losing entries.
        if name.chars().any(char::is_control) {
            body.push_str(&json!(name).to_string());
        } else {
            body.push_str(name);
        }
        body.push_str(match entry.kind {
            Kind::Directory => "/",
            Kind::Symlink => "@",
            _ => "",
        });
        body.push('\n');
    }
    let count = page.entries.len();
    let qualifier = if page.next.is_some() || page.scan_capped {
        ">="
    } else {
        ""
    };
    body.push_str(&format!(
        "[{qualifier}{count} entries; {count} shown; {} names read{}]",
        page.scanned,
        if page.scan_capped {
            "; scan capped; totals are lower bounds"
        } else {
            ""
        }
    ));
    if let Some(cursor) = &page.next {
        body.push_str(&format!("\nnext cursor: {cursor}"));
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_defaults_validation_and_ignore() {
        let input = Input::parse("{}").unwrap();
        assert_eq!(
            (input.path.as_str(), input.limit, input.depth, input.long),
            (".", 500, 1, false)
        );
        for bad in [
            r#"{"limit":0}"#,
            r#"{"limit":501}"#,
            r#"{"depth":0}"#,
            r#"{"depth":-1}"#,
            r#"{"recursive":true}"#,
        ] {
            assert!(Input::parse(bad).is_err());
        }
        assert_eq!(
            Input::parse(r#"{"ignore":"*.log"}"#).unwrap().patterns(),
            &["*.log"]
        );
        assert_eq!(input_schema()["properties"]["limit"]["maximum"], 500);
    }
    #[test]
    fn footer_honesty_and_symlink_rendering() {
        let page = Page {
            entries: vec![Entry {
                path: "src/link".into(),
                kind: Kind::Symlink,
                size: 999,
                depth: 2,
            }],
            next: Some("opaque".into()),
            scanned: 100_000,
            scan_capped: true,
        };
        let out = render(&page, true);
        assert!(out.starts_with("l        -   link@\n"), "{out}");
        assert!(out.contains(">=1 entries"));
        assert!(out.contains("totals are lower bounds"));
        assert!(out.ends_with("next cursor: opaque"));
    }
}
