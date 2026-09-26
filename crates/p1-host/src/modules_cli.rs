//! `p1 modules`: the module set of a p1 release (ADR-0079, freeze item 6).
//!
//! A release ships one `modules/manifest.json` (format `p1-release-manifest/1`) beside its
//! packages, and `p1-module-runtime` is the one reader of that format and the one verifier
//! of a package, so these commands read the manifest and its components through the runtime
//! rather than parsing the format a second time.
//!
//! - `list` is every installed package with the environments whose `[[tools]]` names it;
//! - `inspect NAME` is its frozen manifest fields, the imports the build recorded for it and
//!   the capabilities this host would link;
//! - `verify` is metadata only — the digest of every component against the manifest, the
//!   frozen fields the loader would refuse, and the identity of each entry — because
//!   ADR-0079 has an installer run it on a staged set before that set replaces the installed
//!   one. It compiles nothing, starts no session and reads no credential and no user
//!   configuration, so it also works in an install that has neither.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use p1_module_protocol::PROTOCOL_VERSION;
use p1_module_runtime::loader::OFFICIAL_NAMESPACE;
use p1_module_runtime::manifest::{ComponentEntry, Digest, ReleaseManifest};
use p1_module_runtime::{Loader, ModuleKind};

use crate::HostDeps;
use crate::cli::{ModulesAction, ModulesOptions};
use crate::run::{EXIT_FAILURE, EXIT_OK, write_stderr, write_stdout};

/// The manifest a module set is read through (ADR-0079).
const MANIFEST_FILE: &str = "manifest.json";
/// The directory inside a share directory that holds the module set (ADR-0079).
const MODULES_DIR: &str = "modules";
/// The key column width of the `inspect` report.
const KEY_WIDTH: usize = 12;

/// Every module class this runtime speaks, as `Loader::load` reads the manifest's `kind`.
/// The loader's own list is private; naming the variants here is what lets `verify` make the
/// same check without reaching the loader, which compiles. A new class is one variant and
/// one entry, and the drift guard `verify_and_the_loader_agree_on_every_manifest_field` fails
/// if this list and the loader's part ways.
const CLASSES: [ModuleKind; 5] = [
    ModuleKind::Tool,
    ModuleKind::Provider,
    ModuleKind::ContextPolicy,
    ModuleKind::AuthorizationPolicy,
    ModuleKind::WorkflowImplementation,
];

/// The capabilities this runtime links: the loader's `LINKABLE_CAPABILITIES`, published in
/// `docs/design/modules/package.md`. `control`, `clock` and `random` are the runtime's own,
/// `process` is the service it adapts, and `summary` is the context policy's (S5, freeze item
/// 13 of `docs/design/modules/wit.md`); every other interface of `modules/capabilities.toml`
/// arrives with the stream that owns its native service. The loader refuses a manifest
/// granting anything else, so `verify` must refuse it too, and the drift guard
/// `verify_and_the_loader_agree_on_every_manifest_field` fails if this list and the loader's
/// part ways.
const LINKABLE: [&str; 5] = ["control", "clock", "random", "process", "summary"];

/// Run one `p1 modules` command.
pub fn modules(deps: &HostDeps, options: &ModulesOptions) -> i32 {
    let root = match share_dir(options) {
        Ok(root) => root,
        Err(message) => return fail(deps, &message),
    };
    match &options.action {
        ModulesAction::List => list(deps, &root),
        ModulesAction::Inspect { name } => inspect(deps, &root, name),
        ModulesAction::Verify => verify(deps, &root),
    }
}

/// The module set `--root` names, or the installed share directory when it is absent.
fn share_dir(options: &ModulesOptions) -> Result<PathBuf, String> {
    if let Some(root) = &options.root {
        return Ok(root.clone());
    }
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot read the path of this p1: {error}"))?;
    let bin = exe
        .parent()
        .ok_or_else(|| format!("the path of this p1, {}, has no directory", exe.display()))?;
    Ok(bin.join("../share/p1"))
}

/// The module set inside `root`: `root` itself when it holds `manifest.json`, else the
/// `modules/` directory below it. A prefix ships the set at `<share>/modules` (ADR-0079)
/// while a staged or a build-tree set is a directory of its own, and a caller that has one
/// of them can pass either.
fn module_set(root: &Path) -> PathBuf {
    let nested = root.join(MODULES_DIR);
    if nested.join(MANIFEST_FILE).is_file() {
        nested
    } else {
        root.to_path_buf()
    }
}

/// The release manifest of the set `root` names, and the directory its paths are relative to.
fn read_set(root: &Path) -> Result<(PathBuf, ReleaseManifest), String> {
    let set = module_set(root);
    let path = set.join(MANIFEST_FILE);
    let manifest = ReleaseManifest::read(&path).map_err(|error| error.to_string())?;
    Ok((set, manifest))
}

/// `p1 modules list`: every installed package, its digest and the environments that name it.
fn list(deps: &HostDeps, root: &Path) -> i32 {
    let (_, manifest) = match read_set(root) {
        Ok(read) => read,
        Err(message) => return fail(deps, &message),
    };
    let selected = match selected_by(deps, &manifest) {
        Ok(selected) => selected,
        Err(message) => return fail(deps, &message),
    };
    let mut rows: Vec<[String; 5]> = Vec::with_capacity(manifest.components().len());
    for entry in manifest.components() {
        let availability = match selected.get(&entry.name) {
            Some(environments) if !environments.is_empty() => {
                format!("selected by {}", environments.join(", "))
            }
            // An entry of `selected` is never empty; a package no environment names is
            // installed but unselected.
            _ => "unselected".to_string(),
        };
        rows.push([
            entry.name.clone(),
            entry.kind.clone(),
            entry.protocol.clone(),
            entry.digest.to_string(),
            availability,
        ]);
    }
    write_stdout(deps, &table(&rows));
    EXIT_OK
}

/// Which environments name each package: their `[[tools]] module = …` entries, read through
/// `p1-assembly`'s own environment loading, so what an environment names stays that loader's
/// rule instead of a second scan of the TOML.
///
/// An environment that does not load fails the listing: the selection column would otherwise
/// report a package as unselected when the environment naming it is the one that broke.
fn selected_by(
    deps: &HostDeps,
    manifest: &ReleaseManifest,
) -> Result<HashMap<String, Vec<String>>, String> {
    let mut selected: HashMap<String, Vec<String>> = HashMap::new();
    for name in crate::models::environment_names(&deps.environment_dirs)? {
        let environment = p1_assembly::load_environment(&name, &deps.environment_dirs)
            .map_err(|error| error.to_string())?;
        for spec in &environment.tools {
            // An environment names a module by its catalog key, the package name without the
            // reserved namespace (`module = "read"`); the full manifest name is accepted too,
            // so an environment may name either form.
            let entry = manifest
                .entry(&spec.module)
                .or_else(|| manifest.entry(&format!("{OFFICIAL_NAMESPACE}/{}", spec.module)));
            if let Some(entry) = entry {
                let names = selected.entry(entry.name.clone()).or_default();
                if !names.contains(&name) {
                    names.push(name.clone());
                }
            }
        }
    }
    for names in selected.values_mut() {
        names.sort();
    }
    Ok(selected)
}

/// The rows as aligned columns, one package per line, trailing spaces trimmed.
fn table(rows: &[[String; 5]]) -> String {
    let mut widths = [0_usize; 5];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(index, cell)| format!("{cell:<width$}", width = widths[index]))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// `p1 modules inspect NAME`: the frozen manifest fields, the loader-built identity, the
/// component's recorded imports and the capabilities this host would link.
fn inspect(deps: &HostDeps, root: &Path, name: &str) -> i32 {
    let (set, manifest) = match read_set(root) {
        Ok(read) => read,
        Err(message) => return fail(deps, &message),
    };
    let Some(entry) = manifest.entry(name).cloned() else {
        return fail(
            deps,
            &format!(
                "module {name} is not in the module set {}; only packages of the release \
                 manifest are installed",
                set.display()
            ),
        );
    };
    // The loader is the one verifier (freeze item 6): it refuses a class, world, protocol or
    // grant this runtime does not speak, checks the digest, compiles the verified bytes and
    // refuses an import its manifest does not grant. Inspecting through it reports what a
    // load would actually link instead of a second opinion about the same manifest.
    let loader = match Loader::new(manifest, &set) {
        Ok(loader) => loader,
        Err(error) => return fail(deps, &error.to_string()),
    };
    let module = match loader.load(name) {
        Ok(module) => module,
        Err(error) => return fail(deps, &error.to_string()),
    };
    let path = set.join(&entry.path);
    let size = match std::fs::metadata(&path) {
        Ok(metadata) => metadata.len(),
        Err(error) => return fail(deps, &format!("cannot read {}: {error}", path.display())),
    };
    let identity = module.identity();
    let mut out = String::new();
    row(&mut out, "name", &entry.name);
    row(&mut out, "kind", &entry.kind);
    row(&mut out, "world", &entry.world);
    row(&mut out, "protocol", &entry.protocol);
    row(&mut out, "variant", &entry.variant);
    row(&mut out, "digest", module.digest());
    row(&mut out, "size", format!("{size} bytes"));
    row(&mut out, "capabilities", entry.capabilities.join(", "));
    // The manifest's grant is already narrowed to its class's allocation by the build
    // (freeze item 13) and the loader refuses a grant this runtime cannot link, so what
    // survives a load is exactly what the host links.
    row(&mut out, "grants", module.capabilities().join(", "));
    row(
        &mut out,
        "identity",
        format!("{}@{}", identity.implementation, identity.variant),
    );
    row(&mut out, "imports", imports(&set, &entry.path));
    write_stdout(deps, &out);
    EXIT_OK
}

/// One `key  value` line of the `inspect` report.
fn row(out: &mut String, key: &str, value: impl std::fmt::Display) {
    let _ = writeln!(out, "{key:<KEY_WIDTH$} {value}");
}

/// The interfaces the component imports, from the `<package>.imports` file the build
/// published beside it (`docs/design/modules/package.md` — build outputs). A release archive
/// ships the component alone, so a staged set has no list to read there: the runtime's loader
/// compiles the component but hands no import list back, and this command never reads the
/// component's own type itself.
fn imports(set: &Path, relative: &str) -> String {
    let file = set.join(relative).with_extension("imports");
    match std::fs::read_to_string(&file) {
        Ok(text) => {
            let names: Vec<&str> = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect();
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        }
        // A missing record is the ordinary case in a release archive, so it names the file
        // the build would have written instead of printing an absolute path.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => format!(
            "not recorded (the module set has no {})",
            Path::new(relative).with_extension("imports").display()
        ),
        Err(error) => format!("not recorded ({error})"),
    }
}

/// `p1 modules verify [--root DIR]`: every package's bytes re-hashed against the manifest,
/// the frozen fields the loader would refuse, and the identity of each entry. Metadata only:
/// nothing is compiled, instantiated or executed, and no session, credential or user
/// configuration is read, because ADR-0079 has an installer run this on a staged set.
fn verify(deps: &HostDeps, root: &Path) -> i32 {
    let (set, manifest) = match read_set(root) {
        Ok(read) => read,
        Err(message) => return fail(deps, &message),
    };
    let mut verified = 0_usize;
    let mut failed = 0_usize;
    let mut identities: HashMap<Digest, String> = HashMap::new();
    let mut out = String::new();
    for entry in manifest.components() {
        match entry_problems(&set, entry, &mut identities) {
            Ok(size) => {
                verified += 1;
                let _ = writeln!(out, "{} ok {} ({size} bytes)", entry.name, entry.digest);
            }
            Err(problems) => {
                failed += 1;
                for problem in problems {
                    let _ = writeln!(out, "{} FAILED {problem}", entry.name);
                }
            }
        }
    }
    let _ = writeln!(out, "modules verify: {verified} ok, {failed} failed");
    write_stdout(deps, &out);
    if failed == 0 { EXIT_OK } else { EXIT_FAILURE }
}

/// One entry's whole verification: the manifest-only checks and the component file, in the
/// order the loader would refuse them. `Ok` is the size of the bytes that passed; `Err` is one
/// line per problem.
fn entry_problems(
    set: &Path,
    entry: &ComponentEntry,
    identities: &mut HashMap<Digest, String>,
) -> Result<u64, Vec<String>> {
    let mut problems = manifest_problems(entry);
    // Identity is the digest (package.md): the same bytes under two names are one module
    // listed twice, which a release must not ship as two.
    match identities.get(&entry.digest) {
        Some(previous) => problems.push(format!(
            "duplicate identity: {} is also {previous}",
            entry.digest
        )),
        None => {
            identities.insert(entry.digest, entry.name.clone());
        }
    }
    match component_file(set, entry) {
        Ok(size) if problems.is_empty() => Ok(size),
        Ok(_) => Err(problems),
        Err(problem) => {
            problems.push(problem);
            Err(problems)
        }
    }
}

/// The checks `Loader::load` makes from the manifest entry alone, before it reads a byte
/// (`docs/design/modules/package.md` — the loader): the class, its world, the protocol major
/// and the grants. `verify` may not compile, and the loader exposes these only on the way to
/// a compile, so they are made here.
fn manifest_problems(entry: &ComponentEntry) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(kind) = CLASSES.into_iter().find(|class| class.name() == entry.kind) else {
        problems.push(format!(
            "kind {} is not a module class this runtime speaks",
            entry.kind
        ));
        return problems;
    };
    let expected = kind.world();
    if entry.world != expected {
        problems.push(format!(
            "world {} is not {expected}, the {} world this runtime speaks",
            entry.world, entry.kind
        ));
    }
    // The protocol's major must be the runtime's, over the `major.minor` shape
    // `docs/design/modules/protocol.md` fixes. This is the loader's own rule, mirrored exactly
    // because the loader's `protocol_major` is private: both parts must be non-empty ASCII
    // digits and the parsed major must be the runtime's. A looser rule would pass a set the
    // loader then refuses — a signed minor such as `1.+5`, say, which `u32::parse` accepts but
    // the loader's digits-only test does not — defeating the installer's pre-commit check.
    let protocol_major = match entry.protocol.split_once('.') {
        Some((major, minor)) if digits(major) && digits(minor) => major.parse::<u32>().ok(),
        _ => None,
    };
    if protocol_major != Some(PROTOCOL_VERSION.major) {
        problems.push(format!(
            "protocol {} is refused, this runtime speaks protocol major {}",
            entry.protocol, PROTOCOL_VERSION.major
        ));
    }
    for capability in &entry.capabilities {
        if !LINKABLE.contains(&capability.as_str()) {
            problems.push(format!(
                "capability {capability} cannot be linked by this runtime"
            ));
        }
    }
    problems
}

/// Whether `text` is a non-empty run of ASCII digits, the loader's own test for one part of a
/// `major.minor` protocol version.
fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// The component file of one entry: the loader's own reading rules (a regular file, never a
/// symlink; read once; the digest of the bytes must be the manifest's) plus the component
/// ABI, and the size of the bytes that passed. At most one problem is returned, in the order
/// the loader would refuse them.
fn component_file(set: &Path, entry: &ComponentEntry) -> Result<u64, String> {
    let path = set.join(&entry.path);
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let bytes =
        std::fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let actual = Digest::of(&bytes);
    if actual != entry.digest {
        return Err(format!(
            "the bytes hash to {actual}, the manifest pins {}",
            entry.digest
        ));
    }
    component_abi(&bytes)?;
    Ok(bytes.len() as u64)
}

/// The ABI wasmtime compiles: a WebAssembly component, never a core module. The header's
/// layer field is what `scripts/build-modules.sh` reads to tell the two apart, and package.md
/// ships components only, so a core module beside a component manifest is a packaging error
/// that needs no compile to see.
fn component_abi(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 8 || &bytes[0..4] != b"\0asm" {
        return Err("the file is not a WebAssembly module: it has no wasm header".to_string());
    }
    let layer = u16::from_le_bytes([bytes[6], bytes[7]]);
    if layer != 1 {
        return Err(format!(
            "the file is not a component: its wasm layer is {layer}, the component layer is 1"
        ));
    }
    Ok(())
}

/// Report `message` on stderr and fail the process, as the other commands do.
fn fail(deps: &HostDeps, message: &str) -> i32 {
    write_stderr(deps, &format!("{message}\n"));
    EXIT_FAILURE
}
