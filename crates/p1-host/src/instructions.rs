//! Standing instructions and a skill index for the top-level agent (issue #129).
//!
//! `--instructions FILE` appends each file to the system prompt; `--skills DIR` lists the
//! `SKILL.md` files under each directory by name and description, so the agent can
//! load one when a task calls for it. Both are opt-in per launch: the owner's rules are
//! private text, so they never reach child workers (a worker's brief says what it needs)
//! and never reach a run whose launcher did not pass them (e.g. a free-model worker).

use std::path::{Path, PathBuf};

/// Per file: a larger file is cut here with a visible notice, so one oversized file
/// cannot crowd the context window.
pub const MAX_FILE_BYTES: usize = 64 * 1024;

/// The text appended to the system prompt, or an empty string when neither flag was
/// given. An instruction file that cannot be read is an error: a lead launched without
/// its rules must not start as if it had them.
pub fn prompt_section(instructions: &[PathBuf], skill_roots: &[PathBuf]) -> Result<String, String> {
    let mut out = String::new();
    if !instructions.is_empty() {
        out.push_str(
            "\n\n# Standing instructions (loaded by p1 from --instructions)\n\
             These files are the operator's standing rules for this session. Follow them as \
             part of this prompt; a later owner instruction overrides them.\n",
        );
        for path in instructions {
            let text = std::fs::read_to_string(path).map_err(|error| {
                format!("--instructions {}: cannot read it: {error}", path.display())
            })?;
            out.push_str(&format!("\n## {}\n\n", path.display()));
            out.push_str(&truncated(&text, MAX_FILE_BYTES));
            if !text.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    let mut skills = Vec::new();
    for root in skill_roots {
        skills.extend(skills_under(root)?);
    }
    if !skills.is_empty() {
        out.push_str(
            "\n\n# Skills (listed by p1 from --skills)\n\
             A skill is a SKILL.md of instructions for one kind of task. When a task matches a \
             skill's description, read that SKILL.md in full before acting (the `read` tool only \
             reaches the workspace, so use the shell: `cat <path>`), then any file it points to.\n\n",
        );
        for skill in &skills {
            out.push_str(&format!(
                "- {} — {} — {}\n",
                skill.name,
                skill.description,
                skill.path.display()
            ));
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Skill {
    name: String,
    description: String,
    path: PathBuf,
}

/// Every `<root>/<dir>/SKILL.md`, sorted by directory name. A missing root is an error;
/// a directory without SKILL.md is not a skill and is skipped.
fn skills_under(root: &Path) -> Result<Vec<Skill>, String> {
    let entries = std::fs::read_dir(root)
        .map_err(|error| format!("--skills {}: cannot list it: {error}", root.display()))?;
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.join("SKILL.md").is_file())
        .collect();
    dirs.sort();
    let mut skills = Vec::with_capacity(dirs.len());
    for dir in dirs {
        let path = dir.join("SKILL.md");
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("--skills: cannot read {}: {error}", path.display()))?;
        let fallback = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (name, description) = front_matter(&text);
        skills.push(Skill {
            name: name.unwrap_or(fallback),
            description: description.unwrap_or_else(|| "(no description)".to_string()),
            path,
        });
    }
    Ok(skills)
}

/// `name:` and `description:` from a leading `---` front-matter block. Values are single
/// lines; surrounding quotes are removed.
fn front_matter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let mut name = None;
    let mut description = None;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            match key.trim() {
                "name" if !value.is_empty() => name = Some(value),
                "description" if !value.is_empty() => description = Some(value),
                _ => {}
            }
        }
    }
    (name, description)
}

/// `text` cut to at most `limit` bytes on a character boundary, with a notice.
fn truncated(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[p1: truncated at {limit} of {} bytes — read the file for the rest]\n",
        &text[..end],
        text.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, relative: &str, text: &str) -> PathBuf {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn no_flags_add_nothing() {
        assert_eq!(prompt_section(&[], &[]).unwrap(), "");
    }

    #[test]
    fn instruction_files_are_appended_in_order_with_their_paths() {
        let dir = tempfile::tempdir().unwrap();
        let global = write(dir.path(), "global/AGENTS.md", "GLOBAL RULE");
        let repo = write(dir.path(), "repo/AGENTS.md", "REPO RULE\n");
        let section = prompt_section(&[global.clone(), repo.clone()], &[]).unwrap();
        let first = section.find(&format!("## {}", global.display())).unwrap();
        let second = section.find(&format!("## {}", repo.display())).unwrap();
        assert!(first < second);
        assert!(section.contains("GLOBAL RULE\n"));
        assert!(section.contains("REPO RULE\n"));
        assert!(!section.contains("# Skills"));
    }

    #[test]
    fn a_missing_instruction_file_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.md");
        let error = prompt_section(&[missing.clone()], &[]).unwrap_err();
        assert!(error.contains(&missing.display().to_string()), "{error}");
    }

    #[test]
    fn an_oversized_file_is_cut_with_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        let big = write(dir.path(), "big.md", &"é".repeat(MAX_FILE_BYTES));
        let section = prompt_section(&[big], &[]).unwrap();
        assert!(section.contains(&format!("[p1: truncated at {MAX_FILE_BYTES} of")));
        assert!(section.len() < MAX_FILE_BYTES + 1024);
    }

    #[test]
    fn skills_are_listed_by_front_matter_sorted_with_their_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("skills");
        write(
            &root,
            "zeta/SKILL.md",
            "---\nname: zeta\ndescription: \"Use for Z work.\"\n---\n# body\n",
        );
        write(
            &root,
            "alpha/SKILL.md",
            "---\nname: alpha\ndescription: Use for A.\n---\n",
        );
        write(&root, "no-front-matter/SKILL.md", "# just a body\n");
        write(&root, "not-a-skill/README.md", "ignored");
        let section = prompt_section(&[], &[root.clone()]).unwrap();
        let alpha = section.find("- alpha — Use for A. — ").unwrap();
        let fallback = section
            .find("- no-front-matter — (no description) — ")
            .unwrap();
        let zeta = section.find("- zeta — Use for Z work. — ").unwrap();
        assert!(alpha < fallback && fallback < zeta, "{section}");
        assert!(section.contains(&root.join("alpha/SKILL.md").display().to_string()));
        assert!(!section.contains("not-a-skill"));
        assert!(
            !section.contains("# body"),
            "bodies load on demand, not in the prompt"
        );
    }

    #[test]
    fn a_missing_skill_root_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = prompt_section(&[], &[dir.path().join("absent")]).unwrap_err();
        assert!(error.contains("--skills"), "{error}");
    }
}
