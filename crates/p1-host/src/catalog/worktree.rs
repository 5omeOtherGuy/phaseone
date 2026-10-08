//! A workflow step's own git worktree (ADR-0073). The engine carries only strings — a
//! slug and the run's base commit; the git work is here, through the `git` CLI.
//!
//! A step's worktree is `<parent of the main worktree>/<main worktree name>-<slug>` on the
//! branch `task/<slug>`, exactly what `scripts/new-worktree.sh` makes. It is made when
//! missing and reused when it is already there; nothing is ever deleted, reset, cleaned or
//! forced — `git worktree remove` stays the owner's step.

#[cfg(feature = "workflows")]
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(feature = "workflows")]
use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(feature = "workflows")]
use p1_contracts::BoxFuture;
#[cfg(feature = "workflows")]
use p1_workflow::{WorktreeHold, WorktreeInfo};

/// Repository, object, ref and configuration redirects inherited from a git hook must
/// not override `dir` for any child command.
const GIT_ENV_REMOVED: [&str; 16] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_CONFIG_COUNT",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_PARAMETERS",
];

/// `git -C <dir> <args>` with stdin closed and [`GIT_ENV_REMOVED`] removed.
fn command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args).stdin(Stdio::null());
    for name in GIT_ENV_REMOVED {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_CONFIG_KEY_")
            || name.to_string_lossy().starts_with("GIT_CONFIG_VALUE_")
        {
            command.env_remove(name);
        }
    }
    command
}

/// Runs [`command`]. `Ok` is its trimmed stdout; `Err` names the command and carries
/// git's trimmed stderr.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = command(dir, args)
        .output()
        .map_err(|error| format!("git {}: {error}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!("git {} failed: {stderr}", args.join(" ")))
    }
}

/// `HEAD` of the checkout at `dir`.
pub(crate) fn head(dir: &Path) -> Result<String, String> {
    git(dir, &["rev-parse", "HEAD"])
}

/// A fresh detached subagent checkout. Never attaches, resets or deletes an
/// existing branch/worktree; completed work remains available for inspection.
pub(crate) fn isolated(workspace: &Path, worker_id: &str) -> Result<PathBuf, String> {
    let base = head(workspace)?;
    let main = list(workspace)?
        .into_iter()
        .next()
        .ok_or("git lists no worktrees")?
        .path;
    let parent = main.parent().ok_or("main worktree has no parent")?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let path = parent.join(format!(
        "p1-subagent-{}-{stamp}-{worker_id}",
        std::process::id()
    ));
    git(
        workspace,
        &[
            "worktree",
            "add",
            "--detach",
            path.to_str().ok_or("worktree path is not UTF-8")?,
            &base,
        ],
    )?;
    Ok(path)
}

/// The base commit of a run in `workspace` (ADR-0073 item 2): its `HEAD`, or `None` when
/// it is not a git repository (or has no commit). Blocking: call it off the executor.
#[cfg(feature = "workflows")]
pub(crate) fn run_base(workspace: &Path) -> Option<String> {
    head(workspace).ok()
}

/// [`run_base`] off the async executor.
#[cfg(feature = "workflows")]
pub(crate) async fn run_base_async(workspace: PathBuf) -> Option<String> {
    tokio::task::spawn_blocking(move || run_base(&workspace))
        .await
        .ok()
        .flatten()
}

/// One entry of `git worktree list --porcelain`.
struct Entry {
    path: PathBuf,
    /// `refs/heads/<name>`, when a branch is checked out.
    #[cfg(feature = "workflows")]
    branch: Option<String>,
}

fn list(run_workspace: &Path) -> Result<Vec<Entry>, String> {
    let text = git(run_workspace, &["worktree", "list", "--porcelain"])?;
    let mut entries = Vec::new();
    for block in text.split("\n\n") {
        let mut path = None;
        #[cfg(feature = "workflows")]
        let mut branch = None;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(value));
            }
            #[cfg(feature = "workflows")]
            if let Some(value) = line.strip_prefix("branch ") {
                branch = Some(value.to_string());
            }
        }
        if let Some(path) = path {
            entries.push(Entry {
                path,
                #[cfg(feature = "workflows")]
                branch,
            });
        }
    }
    Ok(entries)
}

/// Where a step's worktree for `slug` goes, and whether it is already there: the
/// entries of the repository's worktree list.
#[cfg(feature = "workflows")]
struct Located {
    path: PathBuf,
    branch: String,
    entries: Vec<Entry>,
}

#[cfg(feature = "workflows")]
fn locate(run_workspace: &Path, slug: &str) -> Result<Located, String> {
    let entries = list(run_workspace)?;
    let main = entries
        .first()
        .map(|entry| entry.path.clone())
        .ok_or_else(|| "git worktree list names no worktree".to_string())?;
    let (Some(parent), Some(name)) = (main.parent(), main.file_name()) else {
        return Err(format!(
            "the main worktree {} has no parent directory",
            main.display()
        ));
    };
    let mut dir = name.to_os_string();
    dir.push(format!("-{slug}"));
    Ok(Located {
        path: parent.join(dir),
        branch: format!("task/{slug}"),
        entries,
    })
}

/// Makes or reuses the worktree `located` names (ADR-0073 item 3).
#[cfg(feature = "workflows")]
fn prepare(run_workspace: &Path, located: &Located, base: &str) -> Result<WorktreeInfo, String> {
    let Located {
        path,
        branch,
        entries,
    } = located;
    let path_text = path.to_string_lossy().into_owned();
    let branch_ref = format!("refs/heads/{branch}");
    let registered = entries.iter().find(|entry| same_path(&entry.path, path));
    // A registered path is not permission to follow an alias installed at its pathname.
    if path
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
        return Err(format!(
            "{} is a symlink, not the worktree of {branch}",
            path.display()
        ));
    }
    match registered {
        // A registration whose directory is gone — or replaced by a file or a dangling
        // symlink, which git lists as `prunable` all the same: named, and left for the
        // owner's `git worktree prune`.
        Some(_) if !path.is_dir() => {
            return Err(format!(
                "{} is registered but missing (git worktree prune)",
                path.display()
            ));
        }
        // Registration alone does not prove the directory still points into this repository.
        Some(entry) if entry.branch.as_deref() == Some(branch_ref.as_str()) => {
            validate_checkout(run_workspace, path, &branch_ref)?;
        }
        Some(entry) => {
            return Err(format!(
                "{} is a worktree on {}, not on {branch}",
                path.display(),
                entry
                    .branch
                    .as_deref()
                    .map_or("a detached HEAD", |name| name
                        .trim_start_matches("refs/heads/"))
            ));
        }
        // (b) Something else is there.
        None if path.symlink_metadata().is_ok() => {
            return Err(format!(
                "{} exists and is not the worktree of {branch}",
                path.display()
            ));
        }
        None => {
            let exists = |reference: &str| {
                git(
                    run_workspace,
                    &["rev-parse", "--verify", "--quiet", reference],
                )
                .is_ok()
            };
            // The full ref: `origin/<branch>` alone could resolve to a local ref first.
            let remote = format!("refs/remotes/origin/{branch}");
            if exists(&branch_ref) {
                // (c) The branch without its worktree: attach it.
                git(run_workspace, &["worktree", "add", &path_text, branch])?;
            } else if exists(&remote) {
                // (d) No local branch, but origin has it (a resumed run on a fresh
                // clone): origin's work, tracked.
                git(
                    run_workspace,
                    &[
                        "worktree", "add", "--track", "-b", branch, &path_text, &remote,
                    ],
                )?;
            } else {
                // (d) Neither: a new branch from the run's base.
                git(
                    run_workspace,
                    &["worktree", "add", "-b", branch, &path_text, base],
                )?;
            }
        }
    }
    let current_head = head(path)?;
    validate_checkout(run_workspace, path, &branch_ref)?;
    Ok(WorktreeInfo {
        path: path.clone(),
        branch: branch.clone(),
        head: current_head,
    })
}

/// Compare Git's live repository and branch, not just a worktree-list pathname.
#[cfg(feature = "workflows")]
fn validate_checkout(run_workspace: &Path, path: &Path, branch_ref: &str) -> Result<(), String> {
    let common = |dir: &Path| -> Result<PathBuf, String> {
        let directory = git(
            dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        PathBuf::from(directory)
            .canonicalize()
            .map_err(|error| error.to_string())
    };
    let expected = common(run_workspace)?;
    let actual = common(path)?;
    let active_branch = git(path, &["symbolic-ref", "--quiet", "HEAD"])?;
    if actual != expected || active_branch != branch_ref {
        return Err(format!(
            "{} is not the registered worktree on {} in {}",
            path.display(),
            branch_ref.trim_start_matches("refs/heads/"),
            run_workspace.display()
        ));
    }
    Ok(())
}

#[cfg(feature = "workflows")]
fn same_path(listed: &Path, wanted: &Path) -> bool {
    if listed == wanted {
        return true;
    }
    match (listed.canonicalize(), wanted.canonicalize()) {
        (Ok(listed), Ok(wanted)) => listed == wanted,
        _ => false,
    }
}

/// The step worktree for `slug` in the repository of `run_workspace`, made from `base`
/// when missing (ADR-0073 item 3). Blocking. Errors carry git's own words.
#[cfg(feature = "workflows")]
pub(crate) fn ensure(run_workspace: &Path, slug: &str, base: &str) -> Result<WorktreeInfo, String> {
    let located = locate(run_workspace, slug)?;
    prepare(run_workspace, &located, base)
}

/// The worktree paths held by running steps (ADR-0073 item 4). Its one lock also
/// serialises `git worktree add`, so two steps never race to make the same tree.
#[derive(Default)]
#[cfg(feature = "workflows")]
pub(crate) struct Worktrees {
    held: Mutex<HashSet<PathBuf>>,
}

#[cfg(feature = "workflows")]
impl Worktrees {
    fn lock(&self) -> MutexGuard<'_, HashSet<PathBuf>> {
        self.held
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Prepares and holds the worktree for `slug`. `worktree_busy: <slug>` when a running
    /// step holds it; `worktree: <slug>: …` when it cannot be made. Blocking.
    pub(crate) fn acquire(
        self: &Arc<Self>,
        run_workspace: &Path,
        slug: &str,
        base: &str,
    ) -> Result<WorktreeGuard, String> {
        let mut held = self.lock();
        let failed = |error: String| format!("worktree: {slug}: {error}");
        let located = locate(run_workspace, slug).map_err(failed)?;
        if held.contains(&located.path) {
            return Err(format!("worktree_busy: {slug}"));
        }
        let info = ensure(run_workspace, slug, base).map_err(failed)?;
        held.insert(located.path.clone());
        Ok(WorktreeGuard {
            worktrees: self.clone(),
            key: located.path,
            info,
        })
    }
}

/// A held worktree: released when dropped, however the step ended.
#[cfg(feature = "workflows")]
pub(crate) struct WorktreeGuard {
    worktrees: Arc<Worktrees>,
    key: PathBuf,
    info: WorktreeInfo,
}

#[cfg(feature = "workflows")]
impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        self.worktrees.lock().remove(&self.key);
    }
}

#[cfg(feature = "workflows")]
impl WorktreeHold for WorktreeGuard {
    fn info(&self) -> &WorktreeInfo {
        &self.info
    }

    fn settle<'a>(&'a self) -> BoxFuture<'a, Result<WorktreeInfo, String>> {
        Box::pin(async move {
            let path = self.info.path.clone();
            let head = tokio::task::spawn_blocking(move || head(&path))
                .await
                .map_err(|error| error.to_string())??;
            Ok(WorktreeInfo {
                head,
                ..self.info.clone()
            })
        })
    }
}

#[cfg(all(test, feature = "workflows"))]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A repository in a temp dir, `main` checked out in `<tmp>/repo`, one commit.
    struct Repo {
        dir: tempfile::TempDir,
    }

    impl Repo {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = Self { dir };
            std::fs::create_dir(repo.main()).unwrap();
            repo.git(&["init", "-q", "-b", "main"]);
            repo.commit("first");
            repo
        }

        fn main(&self) -> PathBuf {
            self.dir.path().join("repo")
        }

        fn tree(&self, slug: &str) -> PathBuf {
            self.dir.path().join(format!("repo-{slug}"))
        }

        fn git(&self, args: &[&str]) -> String {
            git(&self.main(), args).unwrap()
        }

        fn commit(&self, name: &str) -> String {
            std::fs::write(self.main().join(name), name).unwrap();
            self.git(&["add", name]);
            self.git(&[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                name,
            ]);
            self.git(&["rev-parse", "HEAD"])
        }
    }

    #[test]
    fn isolated_subagents_get_distinct_detached_committed_checkouts() {
        let repo = Repo::new();
        let base = head(&repo.main()).unwrap();
        std::fs::write(repo.main().join("first"), "uncommitted parent edit").unwrap();
        std::fs::write(repo.main().join("untracked"), "parent only").unwrap();
        let first = isolated(&repo.main(), "w1").unwrap();
        let second = isolated(&repo.main(), "w1").unwrap();
        assert_ne!(first, second);
        assert_eq!(head(&first).unwrap(), base);
        assert!(git(&first, &["symbolic-ref", "--quiet", "HEAD"]).is_err());
        assert_eq!(
            std::fs::read_to_string(first.join("first")).unwrap(),
            "first"
        );
        assert!(!first.join("untracked").exists());
        std::fs::write(first.join("first"), "child edit").unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.main().join("first")).unwrap(),
            "uncommitted parent edit"
        );
        assert_eq!(
            std::fs::read_to_string(second.join("first")).unwrap(),
            "first"
        );
        assert!(
            list(&repo.main())
                .unwrap()
                .iter()
                .any(|entry| entry.path == first)
        );
    }

    #[tokio::test]
    async fn nested_starts_use_the_immediate_parents_worktree_and_head() {
        use super::super::subagents::{ConfiguredStart, Subagents};
        use p1_workers::subagents::{Isolation, SubagentRequest};
        use p1_workers::{ChildId, ChildSpec, WorkerError, WorkersStart};

        #[derive(Default)]
        struct Backend(Mutex<Vec<PathBuf>>);
        impl WorkersStart for Backend {
            fn start<'a>(&'a self, spec: ChildSpec) -> BoxFuture<'a, Result<ChildId, WorkerError>> {
                Box::pin(async move {
                    let workspace = spec.workspace.expect("host stamps immediate parent");
                    let path = match spec.options.isolation {
                        Isolation::Shared => workspace,
                        Isolation::Worktree => isolated(&workspace, "nested").unwrap(),
                    };
                    self.0.lock().unwrap().push(path);
                    Ok(ChildId("w2".into()))
                })
            }
        }
        let repo = Repo::new();
        let root_head = head(&repo.main()).unwrap();
        let parent = isolated(&repo.main(), "w1").unwrap();
        std::fs::write(parent.join("first"), "parent's committed change").unwrap();
        git(&parent, &["add", "first"]).unwrap();
        git(
            &parent,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "parent change",
            ],
        )
        .unwrap();
        let parent_head = head(&parent).unwrap();
        assert_ne!(parent_head, root_head);
        std::fs::write(parent.join("prompt.md"), "nested role").unwrap();
        std::fs::write(
            parent.join("subagents.toml"),
            r#"
[[subagents]]
subagent_type = "search"
environment = "reader"
description = "Find code"
prompt_file = "prompt.md"
tools = ["read"]
models = ["reader/model"]
"#,
        )
        .unwrap();
        let backend = Arc::new(Backend::default());
        let start = ConfiguredStart {
            inner: backend.clone(),
            subagents: Arc::new(Subagents::load(std::slice::from_ref(&parent)).unwrap()),
            grant: vec!["read".into()],
            allowed: Some(vec!["search".into()]),
            builtin: false,
            workspace: parent.clone(),
        };
        for isolation in ["shared", "worktree"] {
            let request: SubagentRequest = serde_json::from_value(serde_json::json!({"subagent_type":"search", "task":"inspect", "isolation":isolation})).unwrap();
            start.start_subagent(request).await.unwrap();
        }
        let paths = backend.0.lock().unwrap();
        assert_eq!(paths[0], parent);
        assert_ne!(paths[1], parent);
        assert_eq!(head(&paths[1]).unwrap(), parent_head);
        assert_eq!(head(&repo.main()).unwrap(), root_head);
        assert_eq!(
            std::fs::read_to_string(paths[1].join("first")).unwrap(),
            "parent's committed change"
        );
    }

    #[test]
    fn a_new_worktree_is_made_on_its_branch_from_the_base_even_after_head_moved() {
        let repo = Repo::new();
        let base = repo.git(&["rev-parse", "HEAD"]);
        let moved = repo.commit("second");
        assert_ne!(base, moved);

        let info = ensure(&repo.main(), "212-read-tool", &base).unwrap();
        let tree = repo.tree("212-read-tool");
        assert!(same_path(&info.path, &tree), "{info:?}");
        assert_eq!(info.branch, "task/212-read-tool");
        assert_eq!(info.head, base, "made from the base, not the moved HEAD");
        assert_eq!(
            git(&tree, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(),
            "task/212-read-tool"
        );
        assert!(!tree.join("second").exists());
        assert_eq!(run_base(&repo.main()), Some(moved));

        // From a step worktree, the path is still next to the MAIN worktree.
        let again = ensure(&tree, "212-read-tool", &base).unwrap();
        assert_eq!(again, info);
    }

    #[test]
    fn a_registered_worktree_is_reused_untouched() {
        let repo = Repo::new();
        let base = repo.git(&["rev-parse", "HEAD"]);
        let info = ensure(&repo.main(), "reuse", &base).unwrap();
        std::fs::write(info.path.join("uncommitted.txt"), "work in progress").unwrap();
        std::fs::write(info.path.join("first"), "edited").unwrap();
        let moved = repo.commit("later");

        let again = ensure(&repo.main(), "reuse", &moved).unwrap();
        assert_eq!(again, info, "the same tree, still at its own head");
        assert_eq!(
            std::fs::read_to_string(again.path.join("uncommitted.txt")).unwrap(),
            "work in progress"
        );
        assert_eq!(
            std::fs::read_to_string(again.path.join("first")).unwrap(),
            "edited"
        );
    }

    #[test]
    fn a_foreign_path_is_an_error_naming_path_and_branch() {
        let repo = Repo::new();
        let base = repo.git(&["rev-parse", "HEAD"]);
        let tree = repo.tree("taken");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("mine.txt"), "keep").unwrap();

        let error = ensure(&repo.main(), "taken", &base).unwrap_err();
        assert!(error.contains(&tree.display().to_string()), "{error}");
        assert!(error.contains("task/taken"), "{error}");
        assert_eq!(
            std::fs::read_to_string(tree.join("mine.txt")).unwrap(),
            "keep"
        );
        assert!(
            git(
                &repo.main(),
                &["rev-parse", "--verify", "--quiet", "refs/heads/task/taken"]
            )
            .is_err(),
            "no branch was made"
        );

        // A worktree on another branch at that path is refused too.
        let other = repo.tree("other");
        repo.git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "elsewhere",
            other.to_str().unwrap(),
            &base,
        ]);
        let error = ensure(&repo.main(), "other", &base).unwrap_err();
        assert!(error.contains("elsewhere"), "{error}");
        assert!(error.contains("task/other"), "{error}");
    }

    #[test]
    fn an_existing_branch_is_attached_where_it_is() {
        let repo = Repo::new();
        let base = repo.git(&["rev-parse", "HEAD"]);
        repo.git(&["branch", "task/attach"]);
        let branch_head = repo.git(&["rev-parse", "task/attach"]);
        let moved = repo.commit("newer");

        let info = ensure(&repo.main(), "attach", &moved).unwrap();
        assert_eq!(info.branch, "task/attach");
        assert_eq!(
            info.head, branch_head,
            "the branch's own commit, not the base"
        );
        assert_eq!(info.head, base);
        assert_eq!(repo.git(&["rev-parse", "task/attach"]), branch_head);
    }

    #[test]
    fn an_origin_only_branch_is_attached_with_tracking_a_local_one_wins_and_neither_is_the_base() {
        let repo = Repo::new();
        let base = repo.git(&["rev-parse", "HEAD"]);
        let origin = repo.dir.path().join("origin.git");
        repo.git(&["init", "-q", "--bare", origin.to_str().unwrap()]);
        repo.git(&["remote", "add", "origin", origin.to_str().unwrap()]);
        repo.git(&["branch", "task/both"]);
        let ahead = repo.commit("ahead");
        repo.git(&[
            "push",
            "-q",
            "origin",
            "HEAD:refs/heads/task/remote-only",
            "HEAD:refs/heads/task/both",
        ]);
        repo.git(&["fetch", "-q", "origin"]);
        assert_ne!(base, ahead);

        // Only origin has it: attached at origin's head, tracking origin's branch.
        let info = ensure(&repo.main(), "remote-only", &base).unwrap();
        assert_eq!(info.branch, "task/remote-only");
        assert_eq!(info.head, ahead, "origin's work, not the base");
        assert_eq!(
            repo.git(&["rev-parse", "refs/heads/task/remote-only"]),
            ahead
        );
        assert_eq!(
            git(
                &info.path,
                &[
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}"
                ]
            )
            .unwrap(),
            "origin/task/remote-only"
        );

        // A local branch wins over origin's.
        let info = ensure(&repo.main(), "both", &base).unwrap();
        assert_eq!(info.head, base, "the local branch, not origin's");

        // Neither: the base, with origin configured.
        let info = ensure(&repo.main(), "neither", &base).unwrap();
        assert_eq!(info.head, base);
    }

    #[test]
    fn a_registered_worktree_whose_directory_is_gone_is_named_not_pruned() {
        let repo = Repo::new();
        let base = repo.git(&["rev-parse", "HEAD"]);
        let info = ensure(&repo.main(), "gone", &base).unwrap();
        std::fs::remove_dir_all(repo.tree("gone")).unwrap();

        let worktrees = Arc::new(Worktrees::default());
        let Err(error) = worktrees.acquire(&repo.main(), "gone", &base) else {
            panic!("the directory is gone");
        };
        assert_eq!(
            error,
            format!(
                "worktree: gone: {} is registered but missing (git worktree prune)",
                info.path.display()
            )
        );
        assert!(
            repo.git(&["worktree", "list", "--porcelain"])
                .contains(&format!("worktree {}", info.path.display())),
            "p1 pruned nothing"
        );
    }

    #[test]
    fn a_registration_whose_path_is_a_plain_file_is_named_not_pruned() {
        let repo = Repo::new();
        let base = repo.git(&["rev-parse", "HEAD"]);
        let info = ensure(&repo.main(), "filed", &base).unwrap();
        std::fs::remove_dir_all(repo.tree("filed")).unwrap();
        std::fs::write(repo.tree("filed"), b"not a worktree").unwrap();

        let worktrees = Arc::new(Worktrees::default());
        let Err(error) = worktrees.acquire(&repo.main(), "filed", &base) else {
            panic!("a file sits where the worktree was");
        };
        assert_eq!(
            error,
            format!(
                "worktree: filed: {} is registered but missing (git worktree prune)",
                info.path.display()
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn registered_worktree_replaced_by_symlink_is_refused() {
        let repo = Repo::new();
        let base = repo.git(&["rev-parse", "HEAD"]);
        let info = ensure(&repo.main(), "alias", &base).unwrap();
        let real = repo.dir.path().join("real");
        std::fs::rename(&info.path, &real).unwrap();
        std::os::unix::fs::symlink(&real, &info.path).unwrap();
        let error = ensure(&repo.main(), "alias", &base).unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert!(error.contains(&info.path.display().to_string()), "{error}");
    }

    #[test]
    fn registered_worktree_with_foreign_git_pointer_is_refused() {
        let repo = Repo::new();
        let base = repo.git(&["rev-parse", "HEAD"]);
        let tree = ensure(&repo.main(), "foreign-pointer", &base).unwrap().path;
        let foreign = repo.dir.path().join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        git(&foreign, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(foreign.join("file"), "foreign").unwrap();
        git(&foreign, &["add", "file"]).unwrap();
        git(
            &foreign,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "foreign",
            ],
        )
        .unwrap();
        git(&foreign, &["checkout", "-q", "-b", "task/foreign-pointer"]).unwrap();
        std::fs::write(
            tree.join(".git"),
            format!("gitdir: {}\n", foreign.join(".git").display()),
        )
        .unwrap();
        let error = ensure(&repo.main(), "foreign-pointer", &base).unwrap_err();
        assert!(error.contains("not the registered worktree"), "{error}");
    }

    #[test]
    fn registered_worktree_replaced_by_plain_foreign_checkout_is_refused() {
        let repo = Repo::new();
        let base = repo.git(&["rev-parse", "HEAD"]);
        let tree = ensure(&repo.main(), "replacement", &base).unwrap().path;
        std::fs::rename(&tree, repo.dir.path().join("saved-tree")).unwrap();
        std::fs::create_dir(&tree).unwrap();
        git(&tree, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(tree.join("file"), "foreign").unwrap();
        git(&tree, &["add", "file"]).unwrap();
        git(
            &tree,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "foreign",
            ],
        )
        .unwrap();
        git(&tree, &["checkout", "-q", "-b", "task/replacement"]).unwrap();
        let error = ensure(&repo.main(), "replacement", &base).unwrap_err();
        assert!(error.contains("not the registered worktree"), "{error}");
    }

    #[test]
    fn git_runs_without_the_inherited_repository_variables() {
        let built = command(Path::new("/nowhere"), &["status"]);
        let removed: Vec<_> = built
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_GRAFT_FILE",
            "GIT_REPLACE_REF_BASE",
            "GIT_SHALLOW_FILE",
        ] {
            assert!(
                removed.iter().any(|removed| removed == name),
                "{name}: {removed:?}"
            );
        }
    }

    #[test]
    fn no_repository_is_a_clear_error_and_no_base() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run_base(dir.path()), None);
        let worktrees = Arc::new(Worktrees::default());
        let Err(error) = worktrees.acquire(dir.path(), "x", "0000") else {
            panic!("not a repository");
        };
        assert!(error.starts_with("worktree: x: "), "{error}");
    }

    #[test]
    fn a_held_worktree_is_refused_and_released_when_its_holder_ends() {
        let repo = Repo::new();
        let base = repo.git(&["rev-parse", "HEAD"]);
        let worktrees = Arc::new(Worktrees::default());

        let (held_tx, held_rx) = mpsc::channel();
        let (end_tx, end_rx) = mpsc::channel::<()>();
        let holder = {
            let worktrees = worktrees.clone();
            let main = repo.main();
            let base = base.clone();
            std::thread::spawn(move || {
                let guard = worktrees.acquire(&main, "shared", &base).unwrap();
                held_tx.send(guard.info().path.clone()).unwrap();
                end_rx.recv().unwrap();
                drop(guard);
            })
        };
        let path = held_rx.recv().unwrap();

        let Err(refused) = worktrees.acquire(&repo.main(), "shared", &base) else {
            panic!("held by the other step");
        };
        assert_eq!(refused, "worktree_busy: shared");
        // Another slug is not blocked by the hold.
        let other = worktrees.acquire(&repo.main(), "unrelated", &base).unwrap();
        drop(other);

        end_tx.send(()).unwrap();
        holder.join().unwrap();
        let guard = worktrees.acquire(&repo.main(), "shared", &base).unwrap();
        assert_eq!(guard.info().path, path);
    }
}
