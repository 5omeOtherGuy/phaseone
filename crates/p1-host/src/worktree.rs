//! A workflow step's own git worktree (ADR-0073). The engine carries only strings — a
//! slug and the run's base commit; the git work is here, through the `git` CLI.
//!
//! A step's worktree is `<parent of the main worktree>/<main worktree name>-<slug>` on the
//! branch `task/<slug>`, exactly what `scripts/new-worktree.sh` makes. It is made when
//! missing and reused when it is already there; nothing is ever deleted, reset, cleaned or
//! forced — `git worktree remove` stays the owner's step.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};

use p1_contracts::BoxFuture;
use p1_workflow::{WorktreeHold, WorktreeInfo};

/// Runs `git -C <dir> <args>` with stdin closed. `Ok` is its trimmed stdout; `Err` names
/// the command and carries git's trimmed stderr.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
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

/// The base commit of a run in `workspace` (ADR-0073 item 2): its `HEAD`, or `None` when
/// it is not a git repository (or has no commit). Blocking: call it off the executor.
pub(crate) fn run_base(workspace: &Path) -> Option<String> {
    head(workspace).ok()
}

/// [`run_base`] off the async executor.
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
    branch: Option<String>,
}

fn list(run_workspace: &Path) -> Result<Vec<Entry>, String> {
    let text = git(run_workspace, &["worktree", "list", "--porcelain"])?;
    let mut entries = Vec::new();
    for block in text.split("\n\n") {
        let mut path = None;
        let mut branch = None;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(value));
            } else if let Some(value) = line.strip_prefix("branch ") {
                branch = Some(value.to_string());
            }
        }
        if let Some(path) = path {
            entries.push(Entry { path, branch });
        }
    }
    Ok(entries)
}

/// Where a step's worktree for `slug` goes, and whether it is already there: the
/// entries of the repository's worktree list.
struct Located {
    path: PathBuf,
    branch: String,
    entries: Vec<Entry>,
}

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
fn prepare(run_workspace: &Path, located: &Located, base: &str) -> Result<WorktreeInfo, String> {
    let Located {
        path,
        branch,
        entries,
    } = located;
    let path_text = path.to_string_lossy().into_owned();
    let branch_ref = format!("refs/heads/{branch}");
    let registered = entries.iter().find(|entry| same_path(&entry.path, path));
    match registered {
        // (a) The step's own worktree: reused untouched — a resumed or repaired step.
        Some(entry) if entry.branch.as_deref() == Some(branch_ref.as_str()) => {}
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
            let exists = git(
                run_workspace,
                &["rev-parse", "--verify", "--quiet", &branch_ref],
            )
            .is_ok();
            if exists {
                // (c) The branch without its worktree: attach it.
                git(run_workspace, &["worktree", "add", &path_text, branch])?;
            } else {
                // (d) Neither: a new branch from the run's base.
                git(
                    run_workspace,
                    &["worktree", "add", "-b", branch, &path_text, base],
                )?;
            }
        }
    }
    Ok(WorktreeInfo {
        path: path.clone(),
        branch: branch.clone(),
        head: head(path)?,
    })
}

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
pub(crate) fn ensure(run_workspace: &Path, slug: &str, base: &str) -> Result<WorktreeInfo, String> {
    let located = locate(run_workspace, slug)?;
    prepare(run_workspace, &located, base)
}

/// The worktree paths held by running steps (ADR-0073 item 4). Its one lock also
/// serialises `git worktree add`, so two steps never race to make the same tree.
#[derive(Default)]
pub(crate) struct Worktrees {
    held: Mutex<HashSet<PathBuf>>,
}

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
pub(crate) struct WorktreeGuard {
    worktrees: Arc<Worktrees>,
    key: PathBuf,
    info: WorktreeInfo,
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        self.worktrees.lock().remove(&self.key);
    }
}

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

#[cfg(test)]
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
