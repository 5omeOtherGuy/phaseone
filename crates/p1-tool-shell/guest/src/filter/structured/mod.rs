//! Structured Rust filters for the top shell command classes (research #42):
//! cargo test, cargo build/check/clippy, git status/log/diff, and npm/pnpm/
//! yarn/bun test (jest/vitest).
//!
//! These parse the output before summarizing, so they can produce per-binary
//! test summaries, per-file diff stats, and compact commit lines that line
//! regexes cannot. Copied from the donor `iris-agent`
//! `src/tools/bash/filter/structured/` and adapted to p1: the donor's
//! declarative TOML engine and its registry are not taken, so a matched
//! filter either summarizes or declines to the raw output.
//!
//! Contract per filter (`apply: fn(&str, bool) -> Option<String>`):
//! - `None` means "cannot parse this confidently" and yields the raw output
//!   at the seam -- a structured filter never guesses;
//! - success summaries are only produced when `exit_ok` is true;
//! - failure detail (failing test names, panic messages, `file:line`
//!   references, compiler diagnostics, diff hunks) is kept verbatim;
//! - only known-noise lines are ever dropped on failure paths.

mod cargo_build;
mod cargo_test;
mod git_diff;
mod git_log;
mod git_status;
mod npm_test;

/// A structured filter selected for the effective command.
pub(super) struct StructuredFilter {
    /// `(output, exit_ok) -> Option<filtered>`; `None` = decline (raw).
    pub(super) apply: fn(&str, bool) -> Option<String>,
}

/// Find the structured filter for an effective command (as produced by
/// `command::effective_command`). Matching is token-based and conservative:
/// anything ambiguous returns `None` and the output passes through raw.
pub(super) fn find(effective: &str) -> Option<StructuredFilter> {
    let tokens: Vec<&str> = effective.split_whitespace().collect();
    let (&program, args) = tokens.split_first()?;
    match program {
        "cargo" => {
            // Skip a `+toolchain` selector.
            let args = match args.split_first() {
                Some((t, rest)) if t.starts_with('+') => rest,
                _ => args,
            };
            match args.first().copied()? {
                "t" | "test" => Some(StructuredFilter {
                    apply: cargo_test::apply,
                }),
                // `cargo run` is deliberately uncovered: after the chatter
                // comes arbitrary program output, which must stay raw.
                "b" | "build" | "c" | "check" | "clippy" | "fix" | "doc" => {
                    Some(StructuredFilter {
                        apply: cargo_build::apply,
                    })
                }
                _ => None,
            }
        }
        "git" => match git_subcommand(args)? {
            "status" => Some(StructuredFilter {
                apply: git_status::apply,
            }),
            "log" => Some(StructuredFilter {
                apply: git_log::apply,
            }),
            "diff" => Some(StructuredFilter {
                apply: git_diff::apply,
            }),
            _ => None,
        },
        "npm" | "pnpm" | "yarn" | "bun" => {
            let is_test = match args.first().copied()? {
                "t" | "test" | "tst" => true,
                "run" => args
                    .get(1)
                    .is_some_and(|s| *s == "test" || s.starts_with("test:")),
                _ => false,
            };
            is_test.then_some(StructuredFilter {
                apply: npm_test::apply,
            })
        }
        "npx" => match args.first().copied()? {
            "jest" | "vitest" => Some(StructuredFilter {
                apply: npm_test::apply,
            }),
            _ => None,
        },
        "jest" | "vitest" => Some(StructuredFilter {
            apply: npm_test::apply,
        }),
        _ => None,
    }
}

/// Extract the git subcommand, skipping known global flags. Unknown leading
/// flags return `None` (conservative: never guess the subcommand).
fn git_subcommand<'a>(args: &[&'a str]) -> Option<&'a str> {
    let mut i = 0;
    while let Some(&arg) = args.get(i) {
        if !arg.starts_with('-') {
            return Some(arg);
        }
        match arg {
            // Global flags that consume a separate value token.
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path" => i += 2,
            // Known value-less or `--flag=value` global flags.
            "-P"
            | "--no-pager"
            | "--paginate"
            | "--no-optional-locks"
            | "--literal-pathspecs"
            | "--no-replace-objects"
            | "--bare" => i += 1,
            _ if arg.starts_with("--") && arg.contains('=') => i += 1,
            // Anything else: refuse to guess.
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(effective: &str) -> bool {
        find(effective).is_some()
    }

    #[test]
    fn cargo_dispatch() {
        assert!(matches("cargo test"));
        assert!(matches("cargo t --workspace"));
        assert!(matches("cargo +nightly test"));
        assert!(matches("cargo build --release"));
        assert!(matches("cargo check"));
        assert!(matches("cargo clippy -- -D warnings"));
        // `cargo run` relays arbitrary program output; deliberately unmatched.
        assert!(!matches("cargo run"));
        assert!(!matches("cargo r"));
        // nextest output is not libtest format; deliberately unmatched.
        assert!(!matches("cargo nextest run"));
        assert!(!matches("cargo fmt"));
        assert!(!matches("cargo"));
    }

    #[test]
    fn git_dispatch() {
        assert!(matches("git status"));
        assert!(matches("git -C /some/path status"));
        assert!(matches("git -c color.ui=false log -n 5"));
        assert!(matches("git --no-pager diff HEAD~1"));
        assert!(matches("git log --oneline"));
        assert!(!matches("git commit -m x"));
        // Unknown leading flag: refuse to guess the subcommand.
        assert!(!matches("git --weird-flag status"));
        assert!(!matches("git"));
    }

    #[test]
    fn npm_test_dispatch() {
        assert!(matches("npm test"));
        assert!(matches("npm t"));
        assert!(matches("npm test -- --verbose"));
        assert!(matches("pnpm test"));
        assert!(matches("npm run test"));
        assert!(matches("pnpm run test:unit"));
        assert!(matches("npx vitest run"));
        assert!(matches("npx jest"));
        assert!(matches("vitest run"));
        assert!(!matches("npm install"));
        assert!(!matches("npm run build"));
    }

    #[test]
    fn unrelated_commands_do_not_match() {
        assert!(!matches("ls -la"));
        assert!(!matches("shellcheck x.sh"));
    }
}
