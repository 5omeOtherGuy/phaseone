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
//!   configuration, so it also works in an install that has neither. `--integrity-only` is
//!   its installer mode: a grant this runtime cannot link yet (its native service arrives
//!   with a later stream) is reported and passed over, while every other problem still fails
//!   (S1.6.1).
//! - `precompile` is the release staging step (ADR-0113): it compiles every staged
//!   `packages/<package>/<package>.wasm` ahead of time with this p1's own runtime and writes
//!   `<package>.cwasm` beside it, before `scripts/release-manifest.py` pins both digests.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use p1_module_runtime::LINKABLE_CAPABILITIES;
use p1_module_runtime::loader::{
    OFFICIAL_NAMESPACE, check_component_header, manifest_field_errors,
};
use p1_module_runtime::manifest::{ComponentEntry, Digest, ReleaseManifest};

use crate::HostDeps;
use crate::cli::{ModulesAction, ModulesOptions};
use crate::run::{EXIT_FAILURE, EXIT_OK, write_stderr, write_stdout};

/// The manifest a module set is read through (ADR-0079).
const MANIFEST_FILE: &str = "manifest.json";
/// The directory inside a share directory that holds the module set (ADR-0079).
const MODULES_DIR: &str = "modules";
/// The key column width of the `inspect` report.
const KEY_WIDTH: usize = 12;

/// Run one `p1 modules` command.
pub fn modules(deps: &HostDeps, options: &ModulesOptions) -> i32 {
    let root = match share_dir(options) {
        Ok(root) => root,
        Err(message) => return fail(deps, &message),
    };
    match &options.action {
        ModulesAction::List => list(deps, &root),
        ModulesAction::Inspect { name } => inspect(deps, &root, name),
        ModulesAction::Verify => verify(deps, &root, options.integrity_only),
        ModulesAction::Precompile => precompile(deps, &root),
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
    let loader = match crate::catalog::modules::release_loader(manifest, &set) {
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
    row(
        &mut out,
        "compiled",
        match (&entry.precompiled, module.compiled_ahead_of_time()) {
            (Some(_), true) => "ahead of time (the release's compiled copy)",
            (Some(_), false) => "at load (this runtime cannot use the release's compiled copy)",
            (None, _) => "at load (the module set has no compiled copy)",
        },
    );
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

/// `p1 modules verify [--root DIR] [--integrity-only]`: every package's bytes re-hashed
/// against the manifest, the frozen fields the loader would refuse, and the identity of each
/// entry. Metadata only: nothing is compiled, instantiated or executed, and no session,
/// credential or user configuration is read, because ADR-0079 has an installer run this on a
/// staged set.
///
/// `integrity_only` is the installer's mode (S7.8): a grant this runtime cannot link is one
/// `UNLINKED` line per case and never affects the exit code, so an installer accepts a release
/// whose packages grant `workers-*` or `workflows` before S4.7/S6.7 link them. The checks that
/// do fail — the digest, the frozen manifest fields, the component ABI, a duplicate identity —
/// fail in both modes alike.
fn verify(deps: &HostDeps, root: &Path, integrity_only: bool) -> i32 {
    let (set, manifest) = match read_set(root) {
        Ok(read) => read,
        Err(message) => return fail(deps, &message),
    };
    let mut verified = 0_usize;
    let mut failed = 0_usize;
    let mut identities: HashMap<Digest, String> = HashMap::new();
    let mut out = String::new();
    for entry in manifest.components() {
        let report = entry_problems(&set, entry, &mut identities, integrity_only);
        for capability in &report.unlinked {
            let _ = writeln!(out, "UNLINKED {capability} ({})", entry.name);
        }
        match report.size {
            Some(size) => {
                verified += 1;
                let _ = writeln!(out, "{} ok {} ({size} bytes)", entry.name, entry.digest);
            }
            None => {
                failed += 1;
                for problem in &report.problems {
                    let _ = writeln!(out, "{} FAILED {problem}", entry.name);
                }
            }
        }
    }
    let _ = writeln!(out, "modules verify: {verified} ok, {failed} failed");
    write_stdout(deps, &out);
    if failed == 0 { EXIT_OK } else { EXIT_FAILURE }
}

/// One entry's verification: what it passed with, what it failed on, and the grants this
/// runtime cannot link. `size` is `Some` exactly when nothing failed, so the entry's verdict
/// is the one field the two modes read alike.
struct EntryReport {
    /// The size of the bytes that passed, or `None` when a check failed.
    size: Option<u64>,
    /// One line per problem that fails the entry.
    problems: Vec<String>,
    /// The grants this runtime cannot link, in manifest order. `--integrity-only` prints
    /// them and passes the entry; the default mode states each as a problem instead, so its
    /// output stays what it was before the flag existed.
    unlinked: Vec<String>,
}

/// One entry's whole verification: the manifest-only checks and the component file, in the
/// order the loader would refuse them. With `integrity_only`, a grant this runtime cannot link
/// is reported in [`EntryReport::unlinked`] instead of failing the entry.
fn entry_problems(
    set: &Path,
    entry: &ComponentEntry,
    identities: &mut HashMap<Digest, String>,
    integrity_only: bool,
) -> EntryReport {
    let manifest = manifest_problems(entry);
    let mut problems = manifest.problems;
    let unlinked =
        if integrity_only {
            manifest.unlinked
        } else {
            problems.extend(manifest.unlinked.iter().map(|capability| {
                format!("capability {capability} cannot be linked by this runtime")
            }));
            Vec::new()
        };
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
    let size = match component_file(set, entry) {
        Ok(size) if problems.is_empty() => Some(size),
        Ok(_) => None,
        Err(problem) => {
            problems.push(problem);
            None
        }
    };
    EntryReport {
        size,
        problems,
        unlinked,
    }
}

/// The loader's shared manifest checks before reading a byte, plus its linkable grants.
///
/// The grants are split out from the problems: this runtime's linkable set is the loader's
/// `LINKABLE_CAPABILITIES` today, but a release may ship a package whose granted interface
/// arrives with the native service of a later stream, and `--integrity-only` is the
/// installer's answer to exactly that (S1.6.1). `verify` decides by its flag whether an
/// unlinkable grant fails the run or is only reported.
struct ManifestProblems {
    /// The fields the loader refuses whichever mode runs.
    problems: Vec<String>,
    /// The granted capabilities outside this runtime's linkable set, in manifest order.
    unlinked: Vec<String>,
}

fn manifest_problems(entry: &ComponentEntry) -> ManifestProblems {
    let mut problems = Vec::new();
    let mut unlinked = Vec::new();
    let errors = manifest_field_errors(entry);
    let unknown_kind = errors.iter().any(|error| {
        matches!(
            error,
            p1_module_runtime::loader::LoadError::UnknownKind { .. }
        )
    });
    problems.extend(errors.into_iter().map(|error| error.to_string()));
    if unknown_kind {
        return ManifestProblems { problems, unlinked };
    }
    for capability in &entry.capabilities {
        if !LINKABLE_CAPABILITIES.contains(&capability.as_str()) {
            unlinked.push(capability.clone());
        }
    }
    ManifestProblems { problems, unlinked }
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
    check_component_header(&bytes)?;
    if let Some(precompiled) = &entry.precompiled {
        let path = set.join(&precompiled.path);
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if !metadata.file_type().is_file() {
            return Err(format!("{} is not a regular file", path.display()));
        }
        let compiled = std::fs::read(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let actual = Digest::of(&compiled);
        if actual != precompiled.digest {
            return Err(format!(
                "the compiled copy {} hashes to {actual}, the manifest pins {}",
                precompiled.path, precompiled.digest
            ));
        }
    }
    Ok(bytes.len() as u64)
}

/// `p1 modules precompile --root DIR`: every `packages/<package>/<package>.wasm` below the
/// staged module set `DIR`, compiled ahead of time with this p1's runtime
/// (`p1_module_runtime::precompile`) into `<package>.cwasm` beside it. The release staging
/// runs it with the binary it ships, so the compiled copies come from that binary's own
/// wasmtime and engine configuration; the release manifest written afterwards pins them.
fn precompile(deps: &HostDeps, root: &Path) -> i32 {
    let packages = root.join("packages");
    let mut directories = match std::fs::read_dir(&packages).and_then(|entries| {
        entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
    }) {
        Ok(directories) => directories,
        Err(error) => {
            return fail(
                deps,
                &format!("cannot read {}: {error}", packages.display()),
            );
        }
    };
    directories.sort();
    if directories.is_empty() {
        return fail(
            deps,
            &format!("{}: no packages to compile", packages.display()),
        );
    }
    let mut out = String::new();
    for directory in directories {
        let Some(package) = directory.file_name().and_then(|name| name.to_str()) else {
            return fail(
                deps,
                &format!("{}: not a package name", directory.display()),
            );
        };
        let wasm = directory.join(format!("{package}.wasm"));
        let bytes = match std::fs::symlink_metadata(&wasm) {
            Ok(metadata) if metadata.file_type().is_file() => std::fs::read(&wasm),
            Ok(_) => return fail(deps, &format!("{} is not a regular file", wasm.display())),
            Err(error) => Err(error),
        };
        let compiled = match bytes
            .map_err(|error| format!("cannot read {}: {error}", wasm.display()))
            .and_then(|bytes| {
                p1_module_runtime::precompile(&bytes)
                    .map_err(|error| format!("{}: {error}", wasm.display()))
            }) {
            Ok(compiled) => compiled,
            Err(message) => return fail(deps, &message),
        };
        let cwasm = wasm.with_extension("cwasm");
        if let Err(error) = std::fs::write(&cwasm, &compiled) {
            return fail(deps, &format!("cannot write {}: {error}", cwasm.display()));
        }
        let _ = writeln!(out, "{package} compiled ({} bytes)", compiled.len());
    }
    write_stdout(deps, &out);
    EXIT_OK
}

/// Report `message` on stderr and fail the process, as the other commands do.
fn fail(deps: &HostDeps, message: &str) -> i32 {
    write_stderr(deps, &format!("{message}\n"));
    EXIT_FAILURE
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    // The loader's first-error view, only the parity cases compare against; production
    // accumulates every field problem through `manifest_field_errors` above.
    use p1_module_runtime::loader::check_manifest_fields;

    #[test]
    fn verify_and_loader_share_namespace_and_manifest_checks() {
        let mut entry = ComponentEntry {
            name: "other/read".into(),
            digest: Digest::of(b"x"),
            path: "read.wasm".into(),
            kind: "tool".into(),
            world: "p1:module/tool@1.0.0".into(),
            protocol: "1.0".into(),
            capabilities: vec![],
            variant: "default".into(),
            precompiled: None,
        };
        for name in ["other/read", "p1/read"] {
            entry.name = name.into();
            assert_eq!(
                manifest_problems(&entry).problems.is_empty(),
                check_manifest_fields(&entry).is_ok(),
                "{name}"
            );
        }
        entry.protocol = "1.+5".into();
        assert_eq!(
            manifest_problems(&entry).problems.is_empty(),
            check_manifest_fields(&entry).is_ok()
        );
    }

    #[test]
    fn verify_reports_both_world_and_protocol_errors_without_changing_loader_first_error() {
        let entry = ComponentEntry {
            name: "p1/read".into(),
            digest: Digest::of(b"x"),
            path: "read.wasm".into(),
            kind: "tool".into(),
            world: "wrong".into(),
            protocol: "2.0".into(),
            capabilities: vec![],
            variant: "default".into(),
            precompiled: None,
        };
        let problems = manifest_problems(&entry).problems.join("\n");
        assert!(problems.contains("world wrong"), "{problems}");
        assert!(problems.contains("protocol 2.0"), "{problems}");
        assert!(matches!(
            check_manifest_fields(&entry),
            Err(p1_module_runtime::loader::LoadError::WorldMismatch { .. })
        ));
    }

    #[test]
    fn verify_has_explicit_namespace_and_header_version_verdicts() {
        let mut entry = ComponentEntry {
            name: "other/read".into(),
            digest: Digest::of(b"x"),
            path: "read.wasm".into(),
            kind: "tool".into(),
            world: "p1:module/tool@1.0.0".into(),
            protocol: "1.0".into(),
            capabilities: vec![],
            variant: "default".into(),
            precompiled: None,
        };
        assert!(
            manifest_problems(&entry)
                .problems
                .iter()
                .any(|error| error.contains("reserved p1/"))
        );
        entry.name = "p1/read".into();
        assert!(manifest_problems(&entry).problems.is_empty());
        entry.kind = "plugin".into();
        assert!(!manifest_problems(&entry).problems.is_empty());
        entry.kind = "tool".into();
        entry.protocol = "1.+5".into();
        assert!(!manifest_problems(&entry).problems.is_empty());
        entry.protocol = "1.0".into();
        let scratch = tempfile::tempdir().unwrap();
        let bytes = b"\0asm\x0c\0\x01\0";
        entry.digest = Digest::of(bytes);
        std::fs::write(scratch.path().join(&entry.path), bytes).unwrap();
        assert!(
            component_file(scratch.path(), &entry)
                .unwrap_err()
                .contains("version")
        );
        let mut identities = HashMap::new();
        assert!(
            entry_problems(scratch.path(), &entry, &mut identities, false)
                .size
                .is_none()
        );
    }

    #[test]
    fn verify_hashes_the_compiled_copy_against_its_own_digest() {
        let scratch = tempfile::tempdir().unwrap();
        let component = b"\0asm\x0d\0\x01\0";
        std::fs::write(scratch.path().join("f.wasm"), component).unwrap();
        std::fs::write(scratch.path().join("f.cwasm"), b"compiled").unwrap();
        let entry = |pinned: Digest| ComponentEntry {
            name: "p1/f".into(),
            digest: Digest::of(component),
            path: "f.wasm".into(),
            kind: "tool".into(),
            world: "p1:module/tool@1.0.0".into(),
            protocol: "1.0".into(),
            capabilities: Vec::new(),
            variant: "default".into(),
            precompiled: Some(p1_module_runtime::Precompiled {
                path: "f.cwasm".into(),
                digest: pinned,
            }),
        };
        assert_eq!(
            component_file(scratch.path(), &entry(Digest::of(b"compiled"))),
            Ok(component.len() as u64)
        );
        let error = component_file(scratch.path(), &entry(Digest::of(b"other"))).unwrap_err();
        assert!(error.contains("compiled copy f.cwasm hashes to"), "{error}");
        std::fs::remove_file(scratch.path().join("f.cwasm")).unwrap();
        assert!(
            component_file(scratch.path(), &entry(Digest::of(b"compiled")))
                .unwrap_err()
                .contains("cannot read")
        );
    }

    #[test]
    fn invalid_component_header_version_is_refused_before_compile() {
        let mut bytes = b"\0asm\x0d\0\x01\0".to_vec();
        assert!(check_component_header(&bytes).is_ok());
        bytes[4] = 12;
        assert!(
            check_component_header(&bytes)
                .unwrap_err()
                .contains("version")
        );
    }
}
