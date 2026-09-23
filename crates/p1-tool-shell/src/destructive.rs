//! Conservative, side-effect-free classification of destructive shell calls.
//!
//! This is a small shell lexer rather than a substring search. In particular,
//! operators inside quotes stay in words and command-looking quoted text is
//! never treated as a command.

use std::path::{Component, Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Redirect,
    Separator,
}

pub(super) fn is_destructive(command: &str, workspace: &Path) -> bool {
    let Some(tokens) = lex(command) else {
        // Unbalanced shell syntax is invalid/ambiguous, so use the safe side of
        // the authorization boundary.
        return true;
    };

    for pair in tokens.windows(2) {
        if pair[0] == Token::Redirect
            && let Token::Word(path) = &pair[1]
            && path_writes_outside(path, workspace)
        {
            return true;
        }
    }

    tokens
        .split(|token| *token == Token::Separator)
        .any(|segment| destructive_segment(segment, workspace))
}

fn destructive_segment(tokens: &[Token], workspace: &Path) -> bool {
    let words: Vec<&str> = tokens
        .iter()
        .filter_map(|token| match token {
            Token::Word(word) => Some(word.as_str()),
            _ => None,
        })
        .collect();
    let Some(program_at) = command_index(&words) else {
        return false;
    };
    let program = words[program_at].rsplit('/').next().unwrap_or_default();
    let args = &words[program_at + 1..];

    match program {
        "rm" => {
            let recursive = args.iter().any(|arg| {
                *arg == "--recursive"
                    || arg
                        .strip_prefix('-')
                        .is_some_and(|flags| !flags.starts_with('-') && flags.contains('r'))
            });
            recursive && args.iter().any(|arg| !arg.starts_with('-'))
        }
        "git" => destructive_git(args),
        "tee" => args
            .iter()
            .filter(|arg| !arg.starts_with('-'))
            .any(|path| path_writes_outside(path, workspace)),
        _ => false,
    }
}

fn command_index(words: &[&str]) -> Option<usize> {
    let mut index = 0;
    while let Some(word) = words.get(index) {
        if is_assignment(word) {
            index += 1;
            continue;
        }
        let program = word.rsplit('/').next().unwrap_or_default();
        if matches!(program, "command" | "env" | "nohup" | "sudo") {
            index += 1;
            while words.get(index).is_some_and(|word| {
                word.starts_with('-') || (program == "env" && is_assignment(word))
            }) {
                index += 1;
            }
            continue;
        }
        return Some(index);
    }
    None
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
}

fn destructive_git(args: &[&str]) -> bool {
    let Some((subcommand_at, subcommand)) = args
        .iter()
        .enumerate()
        .find(|(_, arg)| matches!(**arg, "push" | "reset" | "clean"))
    else {
        return false;
    };
    let options = &args[subcommand_at + 1..];
    match *subcommand {
        "push" => options.iter().any(|arg| {
            *arg == "--force"
                || arg
                    .strip_prefix('-')
                    .is_some_and(|flags| !flags.starts_with('-') && flags.contains('f'))
        }),
        "reset" => options.contains(&"--hard"),
        "clean" => options.iter().any(|arg| {
            arg.strip_prefix('-')
                .is_some_and(|flags| !flags.starts_with('-') && flags.contains('f'))
        }),
        _ => false,
    }
}

fn path_writes_outside(path: &str, workspace: &Path) -> bool {
    if path.is_empty()
        || path.starts_with('&')
        || matches!(path, "/dev/null" | "/dev/stdout" | "/dev/stderr")
    {
        return false;
    }
    if path == "~" || path.starts_with("~/") {
        return true;
    }

    let workspace = normalize(workspace);
    let candidate = if Path::new(path).is_absolute() {
        normalize(Path::new(path))
    } else {
        normalize(&workspace.join(path))
    };
    !candidate.starts_with(workspace)
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn lex(command: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut chars = command.chars().peekable();
    let mut single = false;
    let mut double = false;

    while let Some(ch) = chars.next() {
        if single {
            if ch == '\'' {
                single = false;
            } else {
                word.push(ch);
            }
            continue;
        }
        if ch == '\\' {
            word.push(chars.next()?);
            continue;
        }
        if ch == '\'' && !double {
            single = true;
            continue;
        }
        if ch == '"' {
            double = !double;
            continue;
        }
        if double {
            word.push(ch);
            continue;
        }
        match ch {
            ' ' | '\t' | '\r' => push_word(&mut tokens, &mut word),
            '\n' | ';' | '|' => {
                push_word(&mut tokens, &mut word);
                tokens.push(Token::Separator);
                if chars.peek() == Some(&ch) {
                    chars.next();
                }
            }
            '&' => {
                push_word(&mut tokens, &mut word);
                if chars.peek() == Some(&'>') {
                    chars.next();
                    if chars.peek() == Some(&'>') {
                        chars.next();
                    }
                    tokens.push(Token::Redirect);
                } else {
                    if chars.peek() == Some(&'&') {
                        chars.next();
                    }
                    tokens.push(Token::Separator);
                }
            }
            '>' => {
                // A leading decimal word is an fd selector, not a path.
                if !word.is_empty() && !word.chars().all(|c| c.is_ascii_digit()) {
                    push_word(&mut tokens, &mut word);
                } else {
                    word.clear();
                }
                if chars.peek() == Some(&'>') {
                    chars.next();
                }
                tokens.push(Token::Redirect);
            }
            '<' => {
                push_word(&mut tokens, &mut word);
                if chars.peek() == Some(&'<') {
                    chars.next();
                }
            }
            '(' | ')' => {
                push_word(&mut tokens, &mut word);
                tokens.push(Token::Separator);
            }
            _ => word.push(ch),
        }
    }
    if single || double {
        return None;
    }
    push_word(&mut tokens, &mut word);
    Some(tokens)
}

fn push_word(tokens: &mut Vec<Token>, word: &mut String) {
    if !word.is_empty() {
        tokens.push(Token::Word(std::mem::take(word)));
    }
}

#[cfg(test)]
mod tests {
    use super::is_destructive;
    use std::path::Path;

    #[test]
    fn destructive_commands_from_real_calls_are_flagged() {
        let root = Path::new("/work/project");
        for command in [
            "rm -rf target/",
            "rm -r crates/generated",
            "sudo rm -fr /tmp/generated",
            "git push origin main --force",
            "git push -f",
            "git reset --hard HEAD~1",
            "git clean -fd",
            "cargo test > /tmp/test.log",
            "echo result >> ../result.txt",
            "cargo test | tee -a /var/tmp/test.log",
            "echo x > /work/project/../outside",
        ] {
            assert!(is_destructive(command, root), "{command:?}");
        }
    }

    #[test]
    fn ordinary_and_quoted_commands_are_not_flagged() {
        let root = Path::new("/work/project");
        for command in [
            "grep -r needle target/",
            "rm target/file",
            "git push origin main",
            "git reset --soft HEAD~1",
            "git clean -n",
            "echo result > target/result.txt",
            "cargo test | tee target/test.log",
            "echo 'rm -rf /'",
            "printf '%s' 'git push --force'",
            "echo '> /etc/passwd'",
            "echo 'tee /tmp/file'",
            "echo ok > /dev/null",
        ] {
            assert!(!is_destructive(command, root), "{command:?}");
        }
    }
}
