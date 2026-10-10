//! Filesystem source: one immutable snapshot per assembly, never executable skills.
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use p1_contracts::skill::{LoadedSkill, SkillError, SkillListing, SkillSource, SkillSummary};
use p1_workspace::{CredentialPolicy, open_prompt_file};

mod front_matter;
use front_matter::read_front_matter;

const MAX_FILE_BYTES: usize = 1024 * 1024;

pub struct FilesystemSkills {
    listing: SkillListing,
    bodies: Vec<LoadedSkill>,
}

impl FilesystemSkills {
    pub fn discover(
        workspace: &Path,
        home: Option<&Path>,
        roots: &[String],
        credential_paths: &[PathBuf],
    ) -> Self {
        let mut source = Self {
            listing: SkillListing::default(),
            bodies: Vec::new(),
        };
        let policy = CredentialPolicy::new(home, credential_paths);
        let mut directories: Vec<_> = workspace
            .ancestors()
            .take_while(|path| !path.join(".git").exists())
            .map(Path::to_path_buf)
            .collect();
        if let Some(root) = workspace
            .ancestors()
            .find(|path| path.join(".git").exists())
        {
            directories.push(root.to_path_buf());
            directories.reverse();
        } else {
            directories = vec![workspace.to_path_buf()];
        }
        let mut expanded = Vec::new();
        for root in roots
            .iter()
            .filter(|root| root.starts_with("~/") || Path::new(root).is_absolute())
        {
            if let Some(relative) = root.strip_prefix("~/") {
                if let Some(home) = home {
                    expanded.push(home.join(relative));
                }
            } else {
                expanded.push(PathBuf::from(root));
            }
        }
        for root in roots
            .iter()
            .filter(|root| !root.starts_with("~/") && !Path::new(root).is_absolute())
        {
            expanded.extend(directories.iter().map(|dir| dir.join(root)));
        }
        let mut names = HashSet::new();
        for root in expanded {
            let entries = match std::fs::read_dir(&root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    source
                        .listing
                        .warnings
                        .push(format!("{}: {error}", root.display()));
                    continue;
                }
            };
            let mut directories: Vec<_> = entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect();
            directories.sort();
            for directory in directories {
                if !directory.is_dir() {
                    continue;
                }
                let path = directory.join("SKILL.md");
                match read_skill(&path, &policy) {
                    Ok((name, description, body, bytes, truncated))
                        if names.insert(name.clone()) =>
                    {
                        source.listing.skills.push(SkillSummary {
                            name,
                            description,
                            path,
                            bytes,
                        });
                        source.bodies.push(LoadedSkill {
                            body,
                            directory,
                            truncated,
                        });
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => source
                        .listing
                        .warnings
                        .push(format!("{}: {error}", path.display())),
                }
            }
        }
        source
    }
}

impl SkillSource for FilesystemSkills {
    fn list(&self) -> SkillListing {
        self.listing.clone()
    }
    fn load(&self, name: &str) -> Result<LoadedSkill, SkillError> {
        self.listing
            .skills
            .iter()
            .position(|skill| skill.name == name)
            .map(|index| self.bodies[index].clone())
            .ok_or_else(|| SkillError(format!("unknown skill: {name}")))
    }
}

fn read_skill(
    path: &Path,
    policy: &CredentialPolicy,
) -> std::io::Result<(String, String, String, u64, bool)> {
    let file = open_prompt_file(path, policy)?;
    let size = file.metadata()?.len();
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64).read_to_end(&mut bytes)?;
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if error.utf8_error().error_len().is_none() && size > MAX_FILE_BYTES as u64 => {
            let end = error.utf8_error().valid_up_to();
            String::from_utf8(error.into_bytes()[..end].to_vec()).expect("UTF-8 prefix")
        }
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "skill data is not UTF-8",
            ));
        }
    };
    let truncated = size > text.len() as u64;
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim) != Some("---") {
        return Err(invalid("missing YAML front matter"));
    }
    let start = text.find('\n').map_or(text.len(), |position| position + 1);
    let mut end = start;
    for line in lines {
        if line.trim() == "---" {
            let (name, description) = read_front_matter(&text[start..end]).map_err(invalid)?;
            let name = name.unwrap_or_else(|| {
                path.parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_owned()
            });
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            {
                return Err(invalid(
                    "skill name must be 1-64 lowercase letters, digits or hyphens",
                ));
            }
            if description.trim().is_empty() || description.chars().count() > 1024 {
                return Err(invalid(
                    "skill description must be nonempty and at most 1024 characters",
                ));
            }
            return Ok((
                name,
                description,
                text[end + line.len()..].to_owned(),
                size,
                truncated,
            ));
        }
        end += line.len();
    }
    Err(invalid("unclosed YAML front matter"))
}

fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}
