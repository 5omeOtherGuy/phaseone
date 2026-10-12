//! Kits (ADR-0217 Decision 2): one model family's way of working, the tables of an
//! environment that are not the wire.
//!
//! A kit is catalog content: `kits/<family>/` beside `environments/` in every layer (the user
//! directory, a pack, the shipped catalog; ADR-0201). It holds `kit.toml` and, for one model,
//! `<profile-id>.toml`; `kits/base/kit.toml` is what every family falls back to. For each table
//! the model's file, else the family's `kit.toml`, else `base/kit.toml` supplies it; each of the
//! three files is found across the layers, the first file per relative path winning whole
//! (specificity before layer priority), and a table is taken whole, never merged.
//!
//! Until the prompt profiles of `docs/design/prompts.md` (#779) land, a kit's prompt is a
//! whole file its TOML names with `prompt_file`, relative to that TOML, as `description_file`
//! and `summarize.md` are.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config_reader::ConfigReader;
use crate::{
    AssemblyError, ContextSettings, EnvironmentCapabilities, InstructionSettings, OptionsToml,
    SkillSettings, ToolConcurrencyToml, ToolToml, catalog_sibling,
};

/// The catalog sibling that holds the kits.
const KITS_DIR: &str = "kits";
/// A family's kit file.
const KIT_FILE: &str = "kit.toml";
/// The kit every family falls back to.
const BASE_KIT: &str = "base";

/// One kit file as written: every table an environment carries except `route`, `profile`,
/// `account` and `[[mcp_servers]]`, each optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct KitToml {
    prompt_file: Option<String>,
    #[serde(default)]
    tools: Option<Vec<ToolToml>>,
    #[serde(default)]
    options: Option<OptionsToml>,
    #[serde(default)]
    context: Option<ContextSettings>,
    #[serde(default)]
    tool_concurrency: Option<ToolConcurrencyToml>,
    #[serde(default)]
    instructions: Option<InstructionSettings>,
    #[serde(default)]
    skills: Option<SkillSettings>,
    #[serde(default)]
    capabilities: Option<EnvironmentCapabilities>,
}

/// The kit tables one model resolves to, each with the file that supplied it.
#[derive(Debug, Default)]
pub(crate) struct Kit {
    /// The kit file that names the prompt and the file it names.
    pub prompt: Option<(PathBuf, String)>,
    pub tools: Option<(PathBuf, Vec<ToolToml>)>,
    pub options: Option<(PathBuf, OptionsToml)>,
    pub context: Option<(PathBuf, ContextSettings)>,
    pub tool_concurrency: Option<(PathBuf, ToolConcurrencyToml)>,
    pub instructions: Option<(PathBuf, InstructionSettings)>,
    pub skills: Option<(PathBuf, SkillSettings)>,
    pub capabilities: Option<(PathBuf, EnvironmentCapabilities)>,
}

impl Kit {
    /// The kit of `family` for the model profile `profile`, across the layers whose
    /// environments directories `search_dirs` lists, highest priority first.
    pub(crate) fn resolve(
        search_dirs: &[PathBuf],
        family: &str,
        profile: Option<&str>,
        reader: &ConfigReader,
    ) -> Result<Self, AssemblyError> {
        let mut relative: Vec<PathBuf> = Vec::new();
        if let Some(profile) = profile {
            relative.push(Path::new(family).join(format!("{profile}.toml")));
        }
        relative.push(Path::new(family).join(KIT_FILE));
        if family != BASE_KIT {
            relative.push(Path::new(BASE_KIT).join(KIT_FILE));
        }
        let mut kit = Self::default();
        // Most specific first: a table is taken from the first file that carries it.
        for relative in relative {
            let Some(path) = search_dirs
                .iter()
                .map(|dir| catalog_sibling(dir, KITS_DIR).join(&relative))
                .find(|path| path.is_file())
            else {
                continue;
            };
            let text =
                reader
                    .read(&path)
                    .map_err(|error| AssemblyError::InvalidEnvironmentFile {
                        path: path.clone(),
                        message: error.to_string(),
                    })?;
            let file: KitToml =
                toml::from_str(&text).map_err(|error| AssemblyError::InvalidEnvironmentFile {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
            fill(&mut kit.prompt, &path, file.prompt_file);
            fill(&mut kit.tools, &path, file.tools);
            fill(&mut kit.options, &path, file.options);
            fill(&mut kit.context, &path, file.context);
            fill(&mut kit.tool_concurrency, &path, file.tool_concurrency);
            fill(&mut kit.instructions, &path, file.instructions);
            fill(&mut kit.skills, &path, file.skills);
            fill(&mut kit.capabilities, &path, file.capabilities);
        }
        Ok(kit)
    }
}

/// Keep `slot` when a more specific file already supplied the table, else take `table`.
fn fill<T>(slot: &mut Option<(PathBuf, T)>, path: &Path, table: Option<T>) {
    if slot.is_none()
        && let Some(table) = table
    {
        *slot = Some((path.to_path_buf(), table));
    }
}

/// `name` resolved against the directory of `file`, the file that names it.
pub(crate) fn relative_to(file: &Path, name: &str) -> PathBuf {
    file.parent().unwrap_or(Path::new(".")).join(name)
}
