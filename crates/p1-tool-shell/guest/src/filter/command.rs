//! Effective-command extraction for filter dispatch.
//!
//! Filters are keyed on the parsed program + subcommand of the command that
//! produced the output. The command string the model sends can carry shell
//! plumbing around that program: `cd x && cargo test`, `VAR=1 cargo test`,
//! `sudo systemctl status`, `cargo test 2>&1 | tail`. This module reduces the
//! command string to the last top-level pipeline/sequence segment (the one
//! whose output dominates what was captured) with leading environment
//! assignments and wrapper programs removed.
//!
//! The parse is deliberately conservative: quote- and substitution-aware
//! splitting only, no real shell grammar. Anything ambiguous (unbalanced
//! quotes, empty result) returns `None` and the output passes through
//! unfiltered -- dispatch must never guess.

/// Wrapper programs that run another program and relay its output; skipped
/// (along with their leading flags/assignments) when locating the effective
/// program.
const WRAPPERS: &[&str] = &["sudo", "env", "command", "time", "nohup", "nice", "stdbuf"];

/// Reduce a shell command string to the effective command for filter matching:
/// the last top-level segment, minus env assignments and wrapper programs,
/// re-joined with single spaces. `None` when nothing can be extracted safely.
pub(super) fn effective_command(command: &str) -> Option<String> {
    let segment = last_segment(command)?;
    strip_prefixes(&segment)
}

/// The effective command for the declarative tier, whose regex pipelines and
/// short-circuit messages assume the captured output is ONE program's: only
/// when the last segment alone determines that output (#509). A sequence is
/// accepted when every earlier segment is silent (`cd <dir>`, `export NAME=value`
/// or bare assignments) and joined by `&&`, `;` or a newline, and a subshell is looked through under the
/// same rule; a pipe, `||`, a background `&`, or any earlier segment that can
/// print declines, and the raw output stands.
pub(super) fn declarative_command(command: &str) -> Option<String> {
    let segments = split_segments(command)?;
    let ambiguous = segments.iter().any(|(separator, _)| {
        matches!(
            separator,
            Separator::Pipe | Separator::Or | Separator::Background
        )
    });
    let ((_, last), earlier) = segments.split_last()?;
    if ambiguous || earlier.iter().any(|(_, segment)| !is_silent(segment)) {
        return None;
    }
    if let Some(inner) = last.strip_prefix('(') {
        let inner = inner.strip_suffix(')')?;
        return declarative_command(inner);
    }
    strip_prefixes(last)
}

/// A segment that prints nothing when it succeeds, judged by its form, not its
/// program's name alone: bare assignments, `cd <dir>` (not `cd -`, which prints
/// the directory, nor a flag), or `export` of assignments only (not `export -p`
/// or a bare `export`, which list variables). Anything else may print.
fn is_silent(segment: &str) -> bool {
    let mut tokens = segment
        .split_whitespace()
        .skip_while(|token| is_assignment(token));
    let Some(program) = tokens.next() else {
        return true;
    };
    let args: Vec<&str> = tokens.collect();
    match program {
        "cd" => matches!(args[..], [dir] if !dir.starts_with('-')),
        "export" => !args.is_empty() && args.iter().all(|arg| is_assignment(arg)),
        _ => false,
    }
}

/// `segment` without its leading assignments and wrapper programs.
fn strip_prefixes(segment: &str) -> Option<String> {
    let mut tokens = segment.split_whitespace().peekable();
    // Skip leading `VAR=value` assignments and wrapper programs (with their
    // flags and, for `env`, their own assignments).
    loop {
        let tok = tokens.peek()?;
        if is_assignment(tok) {
            tokens.next();
        } else if WRAPPERS.contains(tok) {
            tokens.next();
            // Consume the wrapper's own flags (`sudo -u user`, `nice -n 10`,
            // `stdbuf -o0`). Flag arguments that don't start with `-` (like the
            // `10` in `nice -n 10`) are not consumed; that conservatively
            // yields a non-match rather than a wrong match.
            while tokens.peek().is_some_and(|t| t.starts_with('-')) {
                tokens.next();
            }
        } else {
            break;
        }
    }
    let rest: Vec<&str> = tokens.collect();
    if rest.is_empty() {
        return None;
    }
    Some(rest.join(" "))
}

fn is_assignment(token: &str) -> bool {
    let Some(eq) = token.find('=') else {
        return false;
    };
    let name = &token[..eq];
    !name.is_empty()
        && name
            .chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
}

/// Split at top-level `;`, `&`, `|`, and newlines (which covers `&&`, `||`,
/// and pipes as empty-segment noise) and return the last non-empty segment.
/// Quote-, escape-, and substitution-aware; `None` on unbalanced quoting.
fn last_segment(command: &str) -> Option<String> {
    let seg = split_segments(command)?.pop()?.1;
    // A parenthesized subshell is transparent for dispatch: recurse into it.
    if let Some(inner) = seg.strip_prefix('(') {
        let inner = inner.strip_suffix(')').unwrap_or(inner);
        return last_segment(inner);
    }
    Some(seg)
}

/// How a segment is joined to the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Separator {
    /// The first segment.
    Start,
    /// `;` or a newline.
    Sequence,
    And,
    Or,
    Pipe,
    Background,
}

/// The non-empty top-level segments of `command`, trimmed, each with the
/// separator before it (the strongest one, when several run together). `None`
/// on unbalanced quoting.
fn split_segments(command: &str) -> Option<Vec<(Separator, String)>> {
    let mut segments: Vec<String> = vec![String::new()];
    let mut separators: Vec<Separator> = vec![Separator::Start];
    let mut chars = command.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0usize; // $( ... ) and ( ... ) nesting
    let mut in_backtick = false;
    let mut prev: Option<char> = None;
    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            push(&mut segments, c);
            prev = Some(c);
            continue;
        }
        match c {
            '\\' => {
                push(&mut segments, c);
                prev = chars.next().inspect(|&next| push(&mut segments, next));
                continue;
            }
            '\'' if !in_double => {
                in_single = true;
                push(&mut segments, c);
            }
            '"' => {
                in_double = !in_double;
                push(&mut segments, c);
            }
            '`' => {
                in_backtick = !in_backtick;
                push(&mut segments, c);
            }
            // Inside quotes a parenthesis is text: it neither opens nor closes a
            // subshell, so it cannot hide the separators after it (#509 repair).
            '(' if !in_double && !in_backtick => {
                depth += 1;
                push(&mut segments, c);
            }
            ')' if !in_double && !in_backtick => {
                depth = depth.saturating_sub(1);
                push(&mut segments, c);
            }
            // `&` in a redirection is not a separator: `2>&1`, `>&2`, `<&0`,
            // `&>log`, `&>>log` all keep the segment intact.
            '&' if !in_double
                && !in_backtick
                && depth == 0
                && (matches!(prev, Some('>' | '<')) || matches!(chars.peek(), Some('>'))) =>
            {
                push(&mut segments, c);
            }
            ';' | '&' | '|' | '\n' if !in_double && !in_backtick && depth == 0 => {
                let separator = match (c, chars.peek()) {
                    ('&', Some('&')) => {
                        chars.next();
                        Separator::And
                    }
                    ('|', Some('|')) => {
                        chars.next();
                        Separator::Or
                    }
                    ('|', _) => Separator::Pipe,
                    ('&', _) => Separator::Background,
                    _ => Separator::Sequence,
                };
                segments.push(String::new());
                separators.push(separator);
            }
            _ => push(&mut segments, c),
        }
        prev = Some(c);
    }
    if in_single || in_double || in_backtick {
        return None; // unbalanced quoting: refuse to guess
    }
    // An empty segment (`a ; ; b`, a trailing `;`) is dropped; its separator
    // folds into the next one, the stronger of the two kept: a pipe or `||`
    // around an empty segment still marks the command ambiguous.
    let mut out: Vec<(Separator, String)> = Vec::new();
    let mut pending = Separator::Start;
    for (separator, segment) in separators.into_iter().zip(segments) {
        pending = pending.max_by_strength(separator);
        let segment = segment.trim();
        if !segment.is_empty() {
            out.push((pending, segment.to_string()));
            pending = Separator::Start;
        }
    }
    Some(out)
}

impl Separator {
    /// The one of `self` and `other` that says more about the shape: any
    /// separator over `Start`, an ambiguous one over a sequence.
    fn max_by_strength(self, other: Self) -> Self {
        let rank = |separator: Self| match separator {
            Separator::Start => 0,
            Separator::Sequence => 1,
            Separator::And => 2,
            Separator::Or | Separator::Pipe | Separator::Background => 3,
        };
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }
}

fn push(segments: &mut [String], c: char) {
    if let Some(last) = segments.last_mut() {
        last.push(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_command_passes_through() {
        assert_eq!(effective_command("cargo test"), Some("cargo test".into()));
    }

    #[test]
    fn cd_prefix_is_dropped() {
        assert_eq!(
            effective_command("cd /some/path && cargo test --workspace"),
            Some("cargo test --workspace".into())
        );
    }

    #[test]
    fn fd_redirections_do_not_split_the_segment() {
        assert_eq!(
            effective_command("cargo test 2>&1"),
            Some("cargo test 2>&1".into())
        );
        assert_eq!(
            effective_command("cargo build &> build.log"),
            Some("cargo build &> build.log".into())
        );
        assert_eq!(effective_command("echo hi >&2"), Some("echo hi >&2".into()));
        // A real background `&` still separates.
        assert_eq!(
            effective_command("sleep 5 & echo done"),
            Some("echo done".into())
        );
        // ... and `&&` still separates.
        assert_eq!(
            effective_command("cd x && cargo test 2>&1"),
            Some("cargo test 2>&1".into())
        );
    }

    #[test]
    fn pipe_tail_wins() {
        // The model already reduced the output itself; the last segment is
        // `tail`, which no filter matches -> passthrough.
        assert_eq!(
            effective_command("cargo test 2>&1 | tail -20"),
            Some("tail -20".into())
        );
    }

    #[test]
    fn env_assignments_are_skipped() {
        assert_eq!(
            effective_command("RUST_BACKTRACE=1 CARGO_TERM_COLOR=never cargo test"),
            Some("cargo test".into())
        );
    }

    #[test]
    fn wrappers_are_skipped() {
        assert_eq!(
            effective_command("sudo systemctl status nginx"),
            Some("systemctl status nginx".into())
        );
        assert_eq!(
            effective_command("env FOO=1 time cargo build"),
            Some("cargo build".into())
        );
    }

    #[test]
    fn wrapper_flag_values_yield_safe_nonmatch() {
        // `-u admin`: the flag's value is not consumed (no per-wrapper flag
        // tables). The leftover prefix matches no filter -> safe passthrough,
        // never a wrong match.
        assert_eq!(
            effective_command("sudo -u admin systemctl status nginx"),
            Some("admin systemctl status nginx".into())
        );
    }

    #[test]
    fn operators_inside_quotes_do_not_split() {
        assert_eq!(
            effective_command("echo \"a && cargo test\""),
            Some("echo \"a && cargo test\"".into())
        );
        assert_eq!(
            effective_command("grep 'foo|bar' file.txt"),
            Some("grep 'foo|bar' file.txt".into())
        );
    }

    #[test]
    fn operators_inside_substitution_do_not_split() {
        assert_eq!(
            effective_command("echo $(git status | wc -l)"),
            Some("echo $(git status | wc -l)".into())
        );
    }

    #[test]
    fn subshell_last_command_is_found() {
        assert_eq!(
            effective_command("(cd sub; cargo test)"),
            Some("cargo test".into())
        );
    }

    #[test]
    fn semicolon_sequence_takes_last() {
        assert_eq!(
            effective_command("git status; git log --oneline"),
            Some("git log --oneline".into())
        );
    }

    #[test]
    fn unbalanced_quote_refuses_to_guess() {
        assert_eq!(effective_command("echo 'unclosed"), None);
    }

    /// #509 item 5: the declarative tier accepts a compound command only when
    /// its last segment alone produces the captured output.
    #[test]
    fn declarative_dispatch_declines_ambiguous_shapes() {
        for (command, expected) in [
            ("helm upgrade app", Some("helm upgrade app")),
            ("helm upgrade app 2>&1", Some("helm upgrade app 2>&1")),
            ("cd sub && helm upgrade app", Some("helm upgrade app")),
            ("export K=1; helm upgrade app", Some("helm upgrade app")),
            (
                "PATH=/b cd sub && K=1 helm upgrade app",
                Some("helm upgrade app"),
            ),
            ("cd sub\nhelm upgrade app", Some("helm upgrade app")),
            ("(cd sub && helm upgrade app)", Some("helm upgrade app")),
            (
                "cd sub && (cd deeper; sudo helm upgrade app)",
                Some("helm upgrade app"),
            ),
            ("git fetch && helm upgrade app", None),
            ("echo hi; helm upgrade app", None),
            ("cat values.yaml | helm upgrade app", None),
            ("helm list |& grep app", None),
            ("false || helm upgrade app", None),
            ("sleep 1 & helm upgrade app", None),
            ("(echo hi; helm upgrade app)", None),
            ("(cd sub) && helm upgrade app", None),
            ("cd sub && (echo hi && helm upgrade app)", None),
            ("cd sub ; | helm upgrade app", None),
            ("echo 'unclosed", None),
        ] {
            assert_eq!(
                declarative_command(command).as_deref(),
                expected,
                "{command}"
            );
        }
        // The structured tier keeps the last-segment rule.
        assert_eq!(
            effective_command("git fetch && helm upgrade app"),
            Some("helm upgrade app".into())
        );
    }

    /// #509 repair 1: silence is judged by form, and quoted or escaped
    /// parentheses and separators neither nest nor split.
    #[test]
    fn only_silent_forms_count_and_quoted_parentheses_do_not_nest() {
        for (command, expected) in [
            ("cd sub && jq . input.json", Some("jq . input.json")),
            ("export A=1 B=2; jq . input.json", Some("jq . input.json")),
            ("A=1; jq . input.json", Some("jq . input.json")),
            ("set; jq . input.json", None),
            ("export -p; jq . input.json", None),
            ("export; jq . input.json", None),
            ("export PATH; jq . input.json", None),
            ("cd - && jq . input.json", None),
            ("cd && jq . input.json", None),
            ("cd -P sub && jq . input.json", None),
            ("unset A; jq . input.json", None),
            ("true; jq . input.json", None),
            ("jq --arg label \"(\" . input.json; seq 1 100", None),
            ("jq --arg label '(' . input.json; seq 1 100", None),
            ("jq --arg label \\( . input.json; seq 1 100", None),
            (
                "cd sub && jq --arg l \")\" . input.json",
                Some("jq --arg l \")\" . input.json"),
            ),
        ] {
            assert_eq!(
                declarative_command(command).as_deref(),
                expected,
                "{command}"
            );
        }
        // The structured tier sees the real last segment too.
        assert_eq!(
            effective_command("echo \"(\"; cargo test"),
            Some("cargo test".into())
        );
    }

    #[test]
    fn empty_and_operator_only_commands_yield_none() {
        assert_eq!(effective_command(""), None);
        assert_eq!(effective_command(" ; ; "), None);
        assert_eq!(effective_command("FOO=bar"), None);
    }
}
