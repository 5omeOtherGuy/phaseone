//! Standing instruction text and per-environment prompt-data settings.
use std::path::{Path, PathBuf};

use crate::config_reader::ConfigReader;
use p1_contracts::skill::SkillSummary;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InstructionSettings {
    pub enabled: bool,
    pub files: Vec<String>,
    pub max_bytes: usize,
}
impl Default for InstructionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            files: vec!["AGENTS.md".into()],
            max_bytes: 32768,
        }
    }
}
impl InstructionSettings {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !(1..=1048576).contains(&self.max_bytes) {
            return Err("instructions.max_bytes must be from 1 to 1048576".into());
        }
        if self.files.iter().any(|name| {
            name.is_empty()
                || Path::new(name).components().count() != 1
                || !matches!(
                    Path::new(name).components().next(),
                    Some(std::path::Component::Normal(_))
                )
        }) {
            return Err("instructions.files must contain file names, not paths".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillSettings {
    pub roots: Vec<String>,
    pub max_listing_chars: usize,
}
impl Default for SkillSettings {
    fn default() -> Self {
        Self {
            roots: vec!["~/.agents/skills".into(), ".agents/skills".into()],
            max_listing_chars: 8000,
        }
    }
}
impl SkillSettings {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !(100..=100000).contains(&self.max_listing_chars) {
            return Err("skills.max_listing_chars must be from 100 to 100000".into());
        }
        Ok(())
    }
}

/// Explicit host locations: never consult the process HOME during assembly.
#[derive(Debug, Clone, Default)]
pub struct InstructionSources {
    pub home: Option<PathBuf>,
    pub global: Option<PathBuf>,
    pub credential_paths: Vec<PathBuf>,
}
#[derive(Debug, Clone, Serialize)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub bytes: u64,
    pub loaded_bytes: usize,
    #[serde(skip)]
    pub text: String,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct InstructionData {
    pub files: Vec<InstructionFile>,
    pub skills: Vec<SkillSummary>,
    pub warnings: Vec<String>,
    pub notice: Option<String>,
}

pub fn expand_home(path: &str, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(relative) = path.strip_prefix("~/") {
        home.map(|home| home.join(relative))
    } else {
        Some(PathBuf::from(path))
    }
}

impl InstructionData {
    pub(crate) fn load(
        workspace: &Path,
        sources: &InstructionSources,
        instructions: &InstructionSettings,
    ) -> Self {
        let mut data = Self::default();
        if !instructions.enabled {
            return data;
        }
        // Worktrees use a .git file; ordinary repositories use a directory.
        let root = workspace
            .ancestors()
            .find(|path| path.join(".git").exists())
            .unwrap_or(workspace);
        let mut dirs: Vec<_> = workspace
            .ancestors()
            .take_while(|path| *path != root)
            .map(Path::to_path_buf)
            .collect();
        dirs.push(root.to_path_buf());
        dirs.reverse();
        let reader = ConfigReader::for_home(sources.home.as_deref(), &sources.credential_paths);
        let mut paths = Vec::new();
        if let Some(global) = &sources.global {
            paths.push(if global.is_absolute() {
                global.clone()
            } else {
                workspace.join(global)
            });
        }
        for dir in &dirs {
            for name in &instructions.files {
                let path = dir.join(name);
                match std::fs::symlink_metadata(&path) {
                    Ok(_) => {
                        paths.push(path);
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        data.warnings.push(format!("{}: {error}", path.display()));
                        break;
                    }
                }
            }
        }
        let mut remaining = instructions.max_bytes;
        for (index, path) in paths.iter().enumerate() {
            if remaining == 0 {
                data.notice = Some(format!(
                    "Instruction byte cap reached; dropped: {}",
                    display_paths(&paths[index..])
                ));
                break;
            }
            match reader.read_prefix(path, remaining) {
                Ok((text, bytes)) => {
                    let truncated = bytes > text.len() as u64;
                    remaining -= text.len();
                    let loaded_bytes = text.len();
                    data.files.push(InstructionFile {
                        path: path.clone(),
                        bytes,
                        loaded_bytes,
                        text,
                    });
                    if truncated {
                        data.notice = Some(format!(
                            "Instruction byte cap reached; truncated: {}; dropped: {}",
                            path.display(),
                            display_paths(&paths[index + 1..])
                        ));
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => data.warnings.push(format!("{}: {error}", path.display())),
            }
        }
        data
    }

    pub(crate) fn append(&self, prompt: &mut String) {
        if !self.files.is_empty() || self.notice.is_some() {
            prompt.push_str("\n\n<instructions>\n");
            for file in &self.files {
                prompt.push_str(&format!(
                    "<instruction_file path=\"{}\">\n{}\n</instruction_file>\n",
                    xml(&file.path.to_string_lossy()),
                    file.text
                ));
            }
            if let Some(notice) = &self.notice {
                prompt.push_str(notice);
                prompt.push('\n');
            }
            prompt.push_str("</instructions>");
        }
    }
}

fn display_paths(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "none".into();
    }
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
