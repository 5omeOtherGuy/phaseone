//! #77: which commands are builds. A recognised build or test command (`cargo …` other than
//! `cargo fmt`, and `scripts/gate.sh`) gets the longer time limits, and only one runs per
//! session: two take the same cargo build lock and the machine's few `rustc-serial` slots, so a
//! second one only waits on the first (the 2026-09-23 log scan lost 2 × 3 600 s to that).
//!
//! The line is read with the destructiveness lexer: every simple command of it counts, so
//! `cd crates/x && cargo test 2>&1 | tail -40` is a build and `echo cargo test` is not. A
//! here-document's body is text, not commands, and a line that does not lex is not a build;
//! bash refuses it anyway.

use std::borrow::Cow;

use crate::destructive;

/// Whether any simple command of `command` is a build or test run.
pub(crate) fn is_build(command: &str) -> bool {
    destructive::commands(&without_heredoc_bodies(command))
        .is_some_and(|commands| commands.iter().any(|words| builds(words)))
}

/// `command` without the bodies of its here-documents (`<<WORD` or `<<-WORD`, the word
/// possibly quoted, up to the line that is the word). A `<<` inside quotes is taken as one
/// too; that only hides text from the classification.
fn without_heredoc_bodies(command: &str) -> Cow<'_, str> {
    if !command.contains("<<") {
        return Cow::Borrowed(command);
    }
    let mut kept = Vec::new();
    let mut lines = command.lines();
    while let Some(line) = lines.next() {
        kept.push(line);
        let Some((_, rest)) = line.split_once("<<") else {
            continue;
        };
        if rest.starts_with('<') {
            // `<<<` is a here-string: no body follows.
            continue;
        }
        let tabs = rest.starts_with('-');
        let delimiter: String = rest
            .trim_start_matches('-')
            .trim_start()
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, ';' | '|' | '&' | ')' | '<' | '>'))
            .filter(|c| !matches!(c, '\'' | '"' | '\\'))
            .collect();
        if delimiter.is_empty() {
            continue;
        }
        for body in lines.by_ref() {
            let end = if tabs {
                body.trim_start_matches('\t')
            } else {
                body
            };
            if end == delimiter {
                kept.push(body);
                break;
            }
        }
    }
    Cow::Owned(kept.join("\n"))
}

fn builds(words: &[String]) -> bool {
    let words = unwrapped(words);
    let Some((program, args)) = words.split_first() else {
        return false;
    };
    match basename(program) {
        // `cargo fmt` neither compiles nor takes the build lock.
        "cargo" => args
            .iter()
            .find(|arg| !arg.starts_with(['-', '+']))
            .is_none_or(|subcommand| subcommand != "fmt"),
        "gate.sh" => true,
        "bash" | "sh" => shell_runs_build(args),
        _ => false,
    }
}

/// `words` after the timing wrappers a build is often run under: `time`, `nice [-n N]` and
/// `timeout [options] DURATION`.
fn unwrapped(mut words: &[String]) -> &[String] {
    loop {
        let Some((program, rest)) = words.split_first() else {
            return words;
        };
        words = match basename(program) {
            "time" => skip_options(rest, &[]),
            "nice" => skip_options(rest, &["-n"]),
            "timeout" => {
                let rest = skip_options(rest, &["-s", "-k", "--signal", "--kill-after"]);
                rest.get(1..).unwrap_or_default()
            }
            _ => return words,
        };
    }
}

/// `words` after its leading options; an option in `with_value` takes the next word too.
fn skip_options<'a>(mut words: &'a [String], with_value: &[&str]) -> &'a [String] {
    while let Some((word, rest)) = words.split_first() {
        if !word.starts_with('-') {
            break;
        }
        words = if with_value.contains(&word.as_str()) {
            rest.get(1..).unwrap_or_default()
        } else {
            rest
        };
    }
    words
}

/// `bash scripts/gate.sh`, or `bash -c '<line>'` whose line runs a build.
fn shell_runs_build(args: &[String]) -> bool {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.strip_prefix('-') {
            Some(flags) if !flags.starts_with('-') && flags.contains('c') => {
                return args.next().is_some_and(|line| is_build(line));
            }
            // `-o option` and `-O shopt_option` take the next word.
            Some("o" | "O") => {
                args.next();
            }
            Some(_) => {}
            None if arg == "+o" || arg == "+O" => {
                args.next();
            }
            None => return basename(arg) == "gate.sh",
        }
    }
    false
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::is_build;

    #[test]
    fn cargo_and_the_gate_are_builds_wherever_they_sit_in_the_line() {
        for command in [
            "cargo test",
            "cargo build --workspace",
            "cargo +nightly clippy -- -D warnings",
            "cargo",
            "/home/u/.cargo/bin/cargo check -p p1-core",
            "cd crates/p1-core && cargo test 2>&1 | tail -40",
            "RUST_LOG=debug cargo test",
            "env CARGO_BUILD_JOBS=3 cargo build",
            "time cargo build",
            "nice -n 10 cargo test",
            "timeout 600 cargo test",
            "timeout -k 5 600 cargo test",
            "timeout --signal KILL 600 cargo test",
            "bash -o pipefail -c 'cargo test | tail'",
            "scripts/gate.sh",
            "./scripts/gate.sh 2>&1 | tail -80",
            "bash scripts/gate.sh",
            "bash -lc 'cargo test -p p1-core'",
            "git stash; cargo test; git stash pop",
            "cat > notes.md <<EOF\nnotes\nEOF\ncargo test",
            "grep x <<< \"$text\"\ncargo build",
        ] {
            assert!(is_build(command), "{command:?} should be a build");
        }
    }

    #[test]
    fn everything_else_is_not_a_build() {
        for command in [
            "echo cargo test",
            "cargo fmt",
            "cargo fmt --all -- --check",
            "cargo +nightly fmt",
            "grep -r cargo .",
            "cat scripts/gate.sh",
            "ls ~/.cargo",
            "bash scripts/pre-commit.sh",
            "bash -lc 'echo hi'",
            "git commit -F - <<'EOF'\nfix x\ncargo test -p p1-core passes\nEOF",
            "cat > notes.md <<-EOF\n\tcargo build\n\tEOF\necho done",
            "cat <<EOF | tee out\ncargo test\nEOF",
            "echo 'unbalanced",
            "",
        ] {
            assert!(!is_build(command), "{command:?} should not be a build");
        }
    }
}
