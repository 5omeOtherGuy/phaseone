//! The bubblewrap sandbox of the process service: what it hides, the `bwrap`
//! argument vector, and the one-time probe that makes an unusable sandbox fail
//! assembly instead of the first command.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

/// Home entries the sandbox leaves visible (read-only) even though it hides the
/// rest of the home directory.
pub const DEFAULT_HOME_VISIBLE: &[&str] = &[
    ".cargo",
    ".rustup",
    ".local/bin",
    ".local/lib",
    ".nvm",
    ".gitconfig",
    ".config/git",
];

/// Home-relative directories [`Sandbox::readable`] must NEVER expose: they hold
/// credentials or agent logins.
pub const CREDENTIAL_DIRECTORIES: &[&str] = &[
    ".ssh",
    ".claude",
    ".codex",
    ".gnupg",
    ".local/share/opencode",
    ".pi",
    ".config/gh",
    ".config/p1",
];

/// What the sandbox hides, keeps visible and keeps writable. The host chooses
/// this; [`ProcessService::sandboxed`](super::ProcessService::sandboxed) turns it
/// into a `bwrap` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    /// The home directory to hide behind a `tmpfs` (canonical once sandboxed).
    pub home: PathBuf,
    /// Paths relative to `home` that stay visible (read-only) if they exist.
    pub home_visible: Vec<PathBuf>,
    /// Extra absolute paths that stay visible READ-ONLY if they exist. A git
    /// worktree keeps its metadata outside the workspace, in the main checkout's
    /// git directory, so a job there needs this to run `git status`/`git diff`.
    /// [`ProcessService::sandboxed`](super::ProcessService::sandboxed) refuses a
    /// path equal to, inside or containing a [`CREDENTIAL_DIRECTORIES`] entry of
    /// the home, and a path containing the home itself.
    pub readable: Vec<PathBuf>,
    /// Extra absolute paths that stay writable if they exist.
    pub writable: Vec<PathBuf>,
    /// A private runtime directory to replace (an empty `tmpfs`), when set and
    /// existing. The host fills it from `XDG_RUNTIME_DIR`; `for_home` leaves it
    /// `None`.
    pub runtime_dir: Option<PathBuf>,
}

impl Sandbox {
    /// A sandbox that hides `home` except for [`DEFAULT_HOME_VISIBLE`].
    pub fn for_home(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            home_visible: DEFAULT_HOME_VISIBLE.iter().map(PathBuf::from).collect(),
            readable: Vec::new(),
            writable: Vec::new(),
            runtime_dir: None,
        }
    }
}

/// Why a sandbox cannot be used. Every message names the remedy: the caller has
/// to be able to act on it, and `--sandbox off` always works.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("bubblewrap (`bwrap`) is not installed: install bubblewrap, or pass --sandbox off")]
    NotInstalled,
    #[error(
        "bubblewrap cannot run here ({0}): enable unprivileged user namespaces, or pass --sandbox off"
    )]
    Unavailable(String),
    #[error(
        "the workspace root {} contains the home directory {}: choose a workspace outside the home, or pass --sandbox off",
        .workspace.display(),
        .home.display()
    )]
    WorkspaceContainsHome { workspace: PathBuf, home: PathBuf },
    #[error(
        "the sandbox readable path {} does not resolve: choose an existing path, or pass --sandbox off",
        .path.display()
    )]
    ReadableUnresolved { path: PathBuf },
    #[error(
        "the sandbox readable path {} resolves under writable root {}: choose another path, or pass --sandbox off",
        .path.display(),
        .root.display()
    )]
    ReadableUnderWritable { path: PathBuf, root: PathBuf },
    #[error(
        "the sandbox readable path {} would uncover the credential directory {}: choose another path, or pass --sandbox off",
        .path.display(),
        .directory.display()
    )]
    ReadableCredential { path: PathBuf, directory: PathBuf },
}

/// The live sandbox: its configuration plus the private `/tmp` the service owns.
pub(super) struct SandboxRuntime {
    pub(super) sandbox: Sandbox,
    pub(super) private_tmp: tempfile::TempDir,
    pub(super) bwrap_path: PathBuf,
}

impl SandboxRuntime {
    /// Check `sandbox` against `workspace` and probe ONCE (`bwrap <args> true`),
    /// so an unusable sandbox fails assembly, not the first command.
    pub(super) fn assemble(sandbox: Sandbox, workspace: &Path) -> Result<Self, SandboxError> {
        let workspace =
            std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        // One canonical home for BOTH the containment check and the mounts: a home
        // reached through a symlink must be hidden at the path bwrap is told about.
        let home = std::fs::canonicalize(&sandbox.home).unwrap_or_else(|_| sandbox.home.clone());
        if home == workspace || home.starts_with(&workspace) {
            return Err(SandboxError::WorkspaceContainsHome { workspace, home });
        }
        let sandbox = Sandbox { home, ..sandbox };
        // Refuse unsafe operator configuration at assembly; bwrap_args repeats
        // this check after resolving each path for every command.
        for readable in &sandbox.readable {
            if let Some(directory) = credential_directory(&sandbox.home, readable) {
                return Err(SandboxError::ReadableCredential {
                    path: readable.clone(),
                    directory,
                });
            }
        }
        let private_tmp = tempfile::Builder::new()
            .prefix("p1-shell-sandbox-")
            .tempdir()
            .map_err(|error| {
                SandboxError::Unavailable(format!("could not create a private /tmp: {error}"))
            })?;
        let writable_roots =
            sandbox_writable_roots(&workspace, &sandbox.writable, private_tmp.path());
        for readable in &sandbox.readable {
            canonical_readable_source(readable, &writable_roots)?;
        }
        let bwrap_path = resolve_bwrap(
            std::env::var_os("PATH").as_deref(),
            &workspace,
            &sandbox.writable,
            private_tmp.path(),
        )
        .ok_or(SandboxError::NotInstalled)?;
        let args = bwrap_args(&sandbox, &workspace, private_tmp.path())?;
        let mut probe = std::process::Command::new(&bwrap_path);
        probe
            .args(&args)
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match probe.output() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(SandboxError::NotInstalled);
            }
            Err(error) => return Err(SandboxError::Unavailable(error.to_string())),
            Ok(output) if !output.status.success() => {
                return Err(SandboxError::Unavailable(
                    String::from_utf8_lossy(&output.stderr).trim().to_string(),
                ));
            }
            Ok(_) => {}
        }
        Ok(Self {
            sandbox,
            private_tmp,
            bwrap_path,
        })
    }
}

/// The argument vector passed to `bwrap` before `bash -lc <command>`.
///
/// The order is part of the contract: later mounts cover earlier ones. Readable
/// paths are resolved and credential-checked at each command start; token masks
/// follow the workspace bind and writable binds. `bwrap` creates mount points.
pub fn bwrap_args(
    sandbox: &Sandbox,
    workspace_root: &Path,
    private_tmp: &Path,
) -> Result<Vec<OsString>, SandboxError> {
    let home = &sandbox.home;
    let writable_roots = sandbox_writable_roots(workspace_root, &sandbox.writable, private_tmp);
    let mut args: Vec<OsString> = Vec::new();
    // 1. The host filesystem, read-only, with fresh /dev and /proc.
    for arg in ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"] {
        args.push(arg.into());
    }
    // 2. A fresh, private /tmp; TMPDIR points every command at it.
    args.push("--bind".into());
    args.push(private_tmp.into());
    args.push("/tmp".into());
    for arg in ["--setenv", "TMPDIR", "/tmp"] {
        args.push(arg.into());
    }
    // 3. Hide the home behind a tmpfs, then put back only what stays visible —
    //    the allow-list entries, then the caller's read-only `readable` paths
    //    (e.g. a git worktree's common directory) — and replace the runtime
    //    directory (agent sockets and keyrings). For a symlinked readable path,
    //    mount its canonical source at the configured destination, which remains
    //    reachable after the home tmpfs hides the original symlink.
    args.push("--tmpfs".into());
    args.push(home.into());
    for entry in &sandbox.home_visible {
        let path = home.join(entry);
        if path.exists() {
            push_ro_bind(&mut args, &path);
        }
    }
    for readable in &sandbox.readable {
        let resolved = canonical_readable_source(readable, &writable_roots)?;
        for candidate in [readable.as_path(), resolved.as_path()] {
            if let Some(directory) = credential_directory(home, candidate) {
                return Err(SandboxError::ReadableCredential {
                    path: readable.clone(),
                    directory,
                });
            }
        }
        args.push("--ro-bind".into());
        args.push(resolved.into());
        args.push(readable.as_os_str().into());
    }
    if let Some(runtime_dir) = &sandbox.runtime_dir
        && runtime_dir.exists()
    {
        args.push("--tmpfs".into());
        args.push(runtime_dir.into());
    }
    // 4. Extra writable paths, if they exist.
    for writable in &sandbox.writable {
        if writable.exists() {
            args.push("--bind".into());
            args.push(writable.into());
            args.push(writable.into());
        }
    }
    // 5. The workspace follows mounts that could cover it; masks then follow
    //    the workspace and writable binds, so neither can uncover credentials.
    args.push("--bind".into());
    args.push(workspace_root.into());
    args.push(workspace_root.into());
    for name in ["credentials.toml", "credentials"] {
        let path = home.join(".cargo").join(name);
        if path.exists() {
            args.push("--ro-bind".into());
            args.push("/dev/null".into());
            args.push(path.into());
        }
    }
    // 6. Only now make the home read-only: writes fail loudly instead of
    //    vanishing into the tmpfs. Child mounts (the workspace) stay writable.
    args.push("--remount-ro".into());
    args.push(home.into());
    // 7. A pid namespace so a detached process still dies with the sandbox.
    for arg in ["--unshare-pid", "--die-with-parent", "--chdir"] {
        args.push(arg.into());
    }
    args.push(workspace_root.into());
    Ok(args)
}

/// Resolve absolute PATH candidates with safe link counts; hard links cannot cross the sandbox's read-only/bind mount boundary.
fn resolve_bwrap(
    path: Option<&OsStr>,
    workspace: &Path,
    writable: &[PathBuf],
    private_tmp: &Path,
) -> Option<PathBuf> {
    let path = path?;
    let forbidden = sandbox_writable_roots(workspace, writable, private_tmp);
    for directory in std::env::split_paths(path) {
        if !directory.is_absolute() {
            continue;
        }
        let Ok(resolved_directory) = std::fs::canonicalize(&directory) else {
            continue;
        };
        if forbidden
            .iter()
            .any(|root| resolved_directory.starts_with(root))
        {
            continue;
        }
        let candidate = resolved_directory.join("bwrap");
        let Ok(resolved) = std::fs::canonicalize(candidate) else {
            continue;
        };
        if forbidden.iter().any(|root| resolved.starts_with(root)) {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&resolved) else {
            continue;
        };
        let trusted_root_owned = metadata.uid() == 0 && metadata.mode() & 0o022 == 0;
        if metadata.is_file()
            && metadata.permissions().mode() & 0o111 != 0
            && (metadata.nlink() == 1 || trusted_root_owned)
        {
            return Some(resolved);
        }
    }
    None
}

/// Canonical destinations outside these roots cannot be redirected by sandboxed writes.
fn sandbox_writable_roots(
    workspace: &Path,
    writable: &[PathBuf],
    private_tmp: &Path,
) -> Vec<PathBuf> {
    let mut roots = vec![
        std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf()),
        std::fs::canonicalize(private_tmp).unwrap_or_else(|_| private_tmp.to_path_buf()),
    ];
    roots.extend(
        writable
            .iter()
            .filter_map(|path| std::fs::canonicalize(path).ok()),
    );
    roots
}

fn canonical_readable_source(
    readable: &Path,
    writable_roots: &[PathBuf],
) -> Result<PathBuf, SandboxError> {
    if !readable.is_absolute() {
        return Err(SandboxError::ReadableUnresolved {
            path: readable.to_path_buf(),
        });
    }
    let resolved =
        std::fs::canonicalize(readable).map_err(|_| SandboxError::ReadableUnresolved {
            path: readable.to_path_buf(),
        })?;
    if let Some(root) = writable_roots
        .iter()
        .find(|root| resolved.starts_with(root))
    {
        return Err(SandboxError::ReadableUnderWritable {
            path: readable.to_path_buf(),
            root: root.clone(),
        });
    }
    Ok(resolved)
}

fn push_ro_bind(args: &mut Vec<OsString>, path: &Path) {
    args.push("--ro-bind".into());
    args.push(path.into());
    args.push(path.into());
}

/// The credential directory of `home` that `readable` would uncover, if any.
///
/// A readable path uncovers a credential directory when it is equal to it, inside
/// it, OR an ancestor of it (including the home itself and `/`): any of the three
/// re-exposes the credentials. Literal and resolved forms are compared, so a
/// symlink cannot smuggle one into view. Existence never matters: a credential
/// directory may be created after the sandbox is assembled.
fn credential_directory(home: &Path, readable: &Path) -> Option<PathBuf> {
    let forms = readable_forms(readable);
    CREDENTIAL_DIRECTORIES.iter().find_map(|entry| {
        let literal = home.join(entry);
        let canonical = std::fs::canonicalize(&literal).unwrap_or_else(|_| literal.clone());
        forms
            .iter()
            .any(|form| {
                form.starts_with(&literal)
                    || form.starts_with(&canonical)
                    || literal.starts_with(form)
                    || canonical.starts_with(form)
            })
            .then_some(literal)
    })
}

/// Every filesystem location `readable` can denote: the path as given, its
/// canonical form when it exists, and the chain of symlink targets, each made
/// absolute and lexically normalised. Following the links by hand (rather than
/// only `canonicalize`, which needs the whole path to exist) catches a symlink
/// whose target is created later. Bounded depth: a symlink loop must not hang.
fn readable_forms(readable: &Path) -> Vec<PathBuf> {
    let mut forms = vec![readable.to_path_buf()];
    let mut current = readable.to_path_buf();
    for _ in 0..8 {
        if let Ok(canonical) = std::fs::canonicalize(&current)
            && !forms.contains(&canonical)
        {
            forms.push(canonical);
        }
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            break;
        };
        if !metadata.file_type().is_symlink() {
            break;
        }
        let Ok(target) = std::fs::read_link(&current) else {
            break;
        };
        let next = lexical_normalize(&if target.is_absolute() {
            target
        } else {
            current.parent().unwrap_or(Path::new("/")).join(target)
        });
        if forms.contains(&next) {
            break;
        }
        forms.push(next.clone());
        current = next;
    }
    forms
}

/// Resolve `.` and `..` lexically, without touching the filesystem (a symlink
/// target may not exist, so `canonicalize` cannot be used).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Sandbox, SandboxError, SandboxRuntime, bwrap_args, resolve_bwrap};
    use std::ffi::OsStr;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;

    fn executable(path: &Path) {
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn resolver_skips_relative_and_workspace_path_entries() {
        let workspace = tempfile::tempdir().unwrap();
        executable(&workspace.path().join("bwrap"));
        let private_tmp = tempfile::tempdir().unwrap();
        let path = std::env::join_paths([OsStr::new("."), workspace.path().as_os_str()]).unwrap();

        assert_eq!(
            resolve_bwrap(Some(&path), workspace.path(), &[], private_tmp.path()),
            None
        );
    }

    #[test]
    fn resolver_returns_an_absolute_executable_for_spawn() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let trusted = temp.path().join("trusted");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&trusted).unwrap();
        executable(&trusted.join("bwrap"));
        let private_tmp = temp.path().join("private-tmp");
        std::fs::create_dir(&private_tmp).unwrap();
        let path = std::env::join_paths([trusted.as_os_str()]).unwrap();

        let program = resolve_bwrap(Some(&path), &workspace, &[], &private_tmp).unwrap();
        assert!(
            program.is_absolute(),
            "spawn must use an absolute program path"
        );
        let metadata = std::fs::metadata(trusted.join("bwrap")).unwrap();
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(
            program,
            std::fs::canonicalize(trusted.join("bwrap")).unwrap()
        );
    }

    #[test]
    fn resolver_skips_user_owned_launcher_with_workspace_hard_link() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let trusted = temp.path().join("trusted");
        let private_tmp = temp.path().join("private-tmp");
        for directory in [&workspace, &trusted, &private_tmp] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let launcher = trusted.join("bwrap");
        executable(&launcher);
        std::fs::hard_link(&launcher, workspace.join("bwrap")).unwrap();
        let metadata = std::fs::metadata(&launcher).unwrap();
        if metadata.uid() == 0 {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o775);
            std::fs::set_permissions(&launcher, permissions).unwrap();
        }
        let path = std::env::join_paths([trusted.as_os_str()]).unwrap();

        assert_eq!(
            resolve_bwrap(Some(&path), &workspace, &[], &private_tmp),
            None
        );
    }

    #[test]
    fn resolver_skips_writable_and_private_tmp_entries_for_safe_absolute_path() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let writable = temp.path().join("writable");
        let private_tmp = temp.path().join("private-tmp");
        let trusted = temp.path().join("trusted");
        for directory in [&workspace, &writable, &private_tmp, &trusted] {
            std::fs::create_dir_all(directory).unwrap();
        }
        executable(&writable.join("bwrap"));
        executable(&private_tmp.join("bwrap"));
        executable(&trusted.join("bwrap"));
        let path = std::env::join_paths([
            writable.as_os_str(),
            private_tmp.as_os_str(),
            trusted.as_os_str(),
        ])
        .unwrap();

        let program = resolve_bwrap(Some(&path), &workspace, &[writable], &private_tmp).unwrap();

        assert_eq!(
            program,
            std::fs::canonicalize(trusted.join("bwrap")).unwrap()
        );
    }

    #[test]
    fn command_time_readable_path_into_credentials_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let credential = home.join(".ssh");
        std::fs::create_dir_all(&credential).unwrap();
        std::fs::write(credential.join("known_hosts"), "not a credential").unwrap();
        let readable = credential.join("known_hosts");
        let sandbox = Sandbox {
            home: home.clone(),
            home_visible: Vec::new(),
            readable: vec![readable],
            writable: Vec::new(),
            runtime_dir: None,
        };

        let result = bwrap_args(&sandbox, temp.path(), &temp.path().join("private-tmp"));
        assert!(matches!(
            result,
            Err(SandboxError::ReadableCredential { .. })
        ));
    }

    #[test]
    fn unresolved_readable_path_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = Sandbox {
            home: temp.path().join("home"),
            home_visible: Vec::new(),
            readable: vec![temp.path().join("does-not-exist")],
            writable: Vec::new(),
            runtime_dir: None,
        };

        let result = bwrap_args(&sandbox, temp.path(), &temp.path().join("private-tmp"));
        assert!(matches!(
            result,
            Err(SandboxError::ReadableUnresolved { .. })
        ));
    }

    #[test]
    fn assembly_rejects_readable_source_under_workspace_before_probe() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let target = workspace.join("readable");
        std::fs::create_dir(&target).unwrap();
        let configured = home.join("workspace-link");
        symlink(&target, &configured).unwrap();
        let sandbox = Sandbox {
            home,
            home_visible: Vec::new(),
            readable: vec![configured],
            writable: Vec::new(),
            runtime_dir: None,
        };

        assert!(matches!(
            SandboxRuntime::assemble(sandbox, &workspace),
            Err(SandboxError::ReadableUnderWritable { .. })
        ));
    }

    #[test]
    fn readable_symlink_into_workspace_is_refused() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let workspace = temp.path().join("workspace");
        let private_tmp = temp.path().join("private-tmp");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&private_tmp).unwrap();
        let target = workspace.join("readable");
        std::fs::create_dir(&target).unwrap();
        let configured = home.join("workspace-link");
        symlink(&target, &configured).unwrap();
        let sandbox = Sandbox {
            home,
            home_visible: Vec::new(),
            readable: vec![configured],
            writable: Vec::new(),
            runtime_dir: None,
        };

        assert!(matches!(
            bwrap_args(&sandbox, &workspace, &private_tmp),
            Err(SandboxError::ReadableUnderWritable { .. })
        ));
    }

    #[test]
    fn readable_symlink_into_writable_root_is_refused() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let workspace = temp.path().join("workspace");
        let writable = temp.path().join("writable");
        let private_tmp = temp.path().join("private-tmp");
        for directory in [&home, &workspace, &writable, &private_tmp] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let target = writable.join("readable");
        std::fs::create_dir(&target).unwrap();
        let configured = home.join("writable-link");
        symlink(&target, &configured).unwrap();
        let sandbox = Sandbox {
            home,
            home_visible: Vec::new(),
            readable: vec![configured],
            writable: vec![writable],
            runtime_dir: None,
        };

        assert!(matches!(
            bwrap_args(&sandbox, &workspace, &private_tmp),
            Err(SandboxError::ReadableUnderWritable { .. })
        ));
    }

    #[test]
    fn readable_directory_outside_writable_roots_is_bound_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let workspace = temp.path().join("workspace");
        let readable = temp.path().join("readable");
        let writable = temp.path().join("writable");
        let private_tmp = temp.path().join("private-tmp");
        for directory in [&home, &workspace, &readable, &writable, &private_tmp] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let sandbox = Sandbox {
            home,
            home_visible: Vec::new(),
            readable: vec![readable.clone()],
            writable: vec![writable],
            runtime_dir: None,
        };

        let args = bwrap_args(&sandbox, &workspace, &private_tmp).unwrap();
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(3).any(|window| {
            window[0] == "--ro-bind"
                && window[1]
                    == std::fs::canonicalize(&readable)
                        .unwrap()
                        .display()
                        .to_string()
                && window[2] == readable.display().to_string()
        }));
    }

    #[test]
    fn readable_symlink_inside_hidden_home_keeps_its_configured_destination() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let source = temp.path().join("shared");
        std::fs::create_dir_all(&source).unwrap();
        let configured = home.join("shared-link");
        symlink(&source, &configured).unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let sandbox = Sandbox {
            home,
            home_visible: Vec::new(),
            readable: vec![configured.clone()],
            writable: Vec::new(),
            runtime_dir: None,
        };

        let args = bwrap_args(&sandbox, &workspace, &temp.path().join("private-tmp")).unwrap();
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(3).any(|window| {
            window[0] == "--ro-bind"
                && window[1]
                    == std::fs::canonicalize(&source)
                        .unwrap()
                        .display()
                        .to_string()
                && window[2] == configured.display().to_string()
        }));
    }

    #[test]
    fn token_masks_follow_workspace_bind() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cargo = home.join(".cargo");
        std::fs::create_dir_all(&cargo).unwrap();
        std::fs::write(cargo.join("credentials"), "not a credential").unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let sandbox = Sandbox {
            home,
            home_visible: Vec::new(),
            readable: Vec::new(),
            writable: Vec::new(),
            runtime_dir: None,
        };

        let args = bwrap_args(&sandbox, &workspace, &temp.path().join("private-tmp")).unwrap();
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let workspace_bind = args
            .windows(3)
            .position(|window| {
                window[0] == "--bind"
                    && window[1] == workspace.display().to_string()
                    && window[2] == workspace.display().to_string()
            })
            .unwrap();
        let mask = args
            .windows(3)
            .position(|window| {
                window[0] == "--ro-bind"
                    && window[1] == "/dev/null"
                    && window[2] == cargo.join("credentials").display().to_string()
            })
            .unwrap();
        assert!(
            mask > workspace_bind,
            "credential mask must follow workspace bind"
        );
    }
}
