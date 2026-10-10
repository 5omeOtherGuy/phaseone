/// Shell constructs whose outer zero status cannot prove the check succeeded.
/// Shell re-entry/expansion is refused rather than trying to parse nested shell syntax.
pub fn is_unprovable(command: &str) -> bool {
    let shell = lex_shell(command);
    shell.opaque
        || shell.tokens.iter().any(|token| {
            matches!(token, ShellToken::Operator(operator) if !matches!(operator.as_str(),
                "&&" | "||" | "|" | "&" | ";" | "\n" | ">" | ">>" | ">|" | ">&" | "&>" | "&>>" | "<" | "<&"))
        })
        || {
            let segments: Vec<&[ShellToken]> = shell
                .tokens
                .split(|token| matches!(token, ShellToken::Operator(operator) if matches!(operator.as_str(), "&&" | "||" | "|" | "&" | ";" | "\n")))
                .collect();
            let last = segments.len().saturating_sub(1);
            segments
                .iter()
                .enumerate()
                .any(|(index, segment)| segment_is_unprovable(segment, index < last))
        }
}

#[derive(Default)]
struct ShellWord {
    text: String,
    quoted: bool,
    expanded: bool,
}

enum ShellToken {
    Word(ShellWord),
    Operator(String),
}

#[derive(Default)]
struct ShellLex {
    tokens: Vec<ShellToken>,
    operator_spans: Vec<std::ops::Range<usize>>,
    opaque: bool,
}

/// Keep word barriers (including empty quotes and escaped operators), so only a
/// backslash-newline can join operators. All command-position checks use these same words.
fn lex_shell(command: &str) -> ShellLex {
    let mut shell = ShellLex {
        opaque: command.contains("$(") || command.contains('`') || command.contains("${"),
        ..ShellLex::default()
    };
    let mut characters = command.char_indices().peekable();
    let mut word: Option<ShellWord> = None;
    let mut quote = None;
    let mut adjacent = false;
    while let Some((offset, character)) = characters.next() {
        if character == '\\' && quote != Some('\'') {
            if characters.peek().is_some_and(|(_, c)| *c == '\n') {
                characters.next();
                continue;
            }
            let word = word.get_or_insert_default();
            match characters.next() {
                Some((_, escaped)) => {
                    // In double quotes only these escapes lose the backslash.
                    if quote == Some('"') && !matches!(escaped, '$' | '`' | '"' | '\\') {
                        word.text.push('\\');
                    }
                    word.text.push(escaped);
                }
                None => shell.opaque = true,
            }
            adjacent = false;
            continue;
        }
        if let Some(open) = quote {
            let word = word.get_or_insert_default();
            if character == open {
                quote = None;
            } else {
                word.text.push(character);
                word.expanded |= open == '"' && character == '$';
            }
            continue;
        }
        match character {
            '\'' | '"' => {
                word.get_or_insert_default().quoted = true;
                quote = Some(character);
                adjacent = false;
            }
            ';' | '\n' | '|' | '&' | '(' | ')' | '{' | '}' | '<' | '>' => {
                if let Some(word) = word.take() {
                    shell.tokens.push(ShellToken::Word(word));
                    adjacent = false;
                }
                if adjacent
                    && character != '\n'
                    && let Some(ShellToken::Operator(operator)) = shell.tokens.last_mut()
                    && !operator.ends_with('\n')
                {
                    operator.push(character);
                    shell.operator_spans.last_mut().unwrap().end = offset + character.len_utf8();
                } else {
                    shell
                        .tokens
                        .push(ShellToken::Operator(character.to_string()));
                    shell
                        .operator_spans
                        .push(offset..offset + character.len_utf8());
                }
                adjacent = true;
            }
            character if character.is_whitespace() => {
                if let Some(word) = word.take() {
                    shell.tokens.push(ShellToken::Word(word));
                }
                adjacent = false;
            }
            _ => {
                // Comments require grammar context; refuse rather than inspect their text.
                shell.opaque |= character == '#' && word.is_none();
                let word = word.get_or_insert_default();
                word.text.push(character);
                word.expanded |= matches!(character, '$' | '*' | '?' | '[')
                    || (character == '~' && word.text.len() == 1);
                adjacent = false;
            }
        }
    }
    if let Some(word) = word {
        shell.tokens.push(ShellToken::Word(word));
    }
    shell.opaque |= quote.is_some();
    shell
}

/// Original spellings of top-level `&&` components, with directory changes retained.
/// Reusing the lexer keeps quoted/escaped operators from becoming separators.
pub(crate) fn chain_components(command: &str) -> Vec<String> {
    let shell = lex_shell(command);
    let mut start = 0;
    let mut parts = Vec::new();
    for (operator, span) in shell
        .tokens
        .iter()
        .filter_map(|token| match token {
            ShellToken::Operator(operator) => Some(operator),
            _ => None,
        })
        .zip(shell.operator_spans)
    {
        if operator == "&&" {
            parts.push(&command[start..span.start]);
            start = span.end;
        }
    }
    parts.push(&command[start..]);
    let mut directory = String::new();
    let mut components = Vec::new();
    for part in parts {
        let part = part.trim();
        let parsed = lex_shell(part);
        let executable = segment_executable(&parsed.tokens);
        if matches!(executable, Some("pushd" | "popd"))
            || (executable == Some("cd") && !part.starts_with("cd "))
        {
            // Do not silently lose an unsupported directory change.
            return Vec::new();
        }
        if part.starts_with("cd ") {
            directory.push_str(part);
            directory.push_str(" && ");
        } else {
            components.push(format!("{directory}{part}"));
        }
    }
    components
}

/// `followed`: another segment comes after this one. A segment that ends the shell
/// (`exec true`, `exit 0`, `builtin exit 0`) skips everything after it, so a later check
/// never runs while the command can still exit 0.
fn segment_is_unprovable(segment: &[ShellToken], followed: bool) -> bool {
    let mut position = 0;
    while let Some(ShellToken::Word(word)) = segment.get(position) {
        if word.quoted || !is_variable_assignment(&word.text) {
            break;
        }
        position += 1;
    }
    while let Some(token) = segment.get(position) {
        let ShellToken::Word(word) = token else {
            // A redirection before the executable makes its position unprovable.
            return true;
        };
        if word.expanded
            || word.text.contains('=')
            || word.text == "!"
            || is_shell_reentry(&word.text)
            || (followed && ends_the_shell(&word.text))
        {
            return true;
        }
        if word
            .text
            .chars()
            .all(|character| character.is_ascii_digit())
            && matches!(segment.get(position + 1), Some(ShellToken::Operator(_)))
        {
            return true;
        }
        if !is_wrapper(&word.text) {
            return false;
        }
        let Some(next) = wrapper_command_position(&word.text, segment, position + 1) else {
            return true;
        };
        position = next;
    }
    false
}

/// Parse only supported wrapper syntax. Once its executable is found, later words
/// are ordinary arguments; unknown options/dispatchers remain conservatively opaque.
fn wrapper_command_position(
    wrapper: &str,
    segment: &[ShellToken],
    mut position: usize,
) -> Option<usize> {
    let wrapper = wrapper.rsplit('/').next()?;
    // These dispatch opaque scripts, aggregate statuses, or can return before a child exits.
    if matches!(wrapper, "xargs" | "parallel" | "su" | "setsid") {
        return None;
    }
    while let Some(ShellToken::Word(word)) = segment.get(position) {
        if word.expanded {
            return None;
        }
        if word.text == "--" {
            position += 1;
            break;
        }
        if !word.text.starts_with('-') || word.text == "-" {
            break;
        }
        let arity = wrapper_option_arity(wrapper, &word.text)?;
        position += 1;
        if arity == 1 {
            let Some(ShellToken::Word(value)) = segment.get(position) else {
                return None;
            };
            if value.expanded {
                return None;
            }
            position += 1;
        }
    }
    if wrapper == "timeout" {
        let Some(ShellToken::Word(duration)) = segment.get(position) else {
            return None;
        };
        if duration.expanded || duration.text.is_empty() {
            return None;
        }
        position += 1;
    }
    // env accepts NAME=VALUE even when NAME is not a valid shell identifier.
    // Unknown sudo assignment syntax stays opaque instead of shifting the executable.
    if matches!(wrapper, "env" | "sudo") {
        while let Some(ShellToken::Word(word)) = segment.get(position) {
            if !word.text.contains('=') {
                break;
            }
            if word.expanded || (wrapper == "sudo" && !is_variable_assignment(&word.text)) {
                return None;
            }
            position += 1;
        }
    }
    match segment.get(position) {
        Some(ShellToken::Word(word)) if word.text.starts_with('-') => None,
        Some(_) => Some(position),
        None => None,
    }
}

fn wrapper_option_arity(wrapper: &str, option: &str) -> Option<usize> {
    let (name, attached) = option
        .split_once('=')
        .map_or((option, false), |(name, _)| (name, true));
    let (flags, values): (&[&str], &[&str]) = match wrapper {
        "env" => (
            &["-i", "--ignore-environment"],
            &["-u", "--unset", "-C", "--chdir"],
        ),
        "timeout" => (
            &["--preserve-status", "--foreground", "-v", "--verbose"],
            &["-s", "--signal", "-k", "--kill-after"],
        ),
        "sudo" => (
            &[
                "-n",
                "--non-interactive",
                "-E",
                "--preserve-env",
                "-H",
                "--set-home",
                "-S",
                "--stdin",
            ],
            &["-u", "--user", "-g", "--group"],
        ),
        "doas" => (&["-n"], &["-u"]),
        "exec" => (&["-c", "-l"], &["-a"]),
        "command" => (&["-p"], &[]),
        "nice" => (&[], &["-n", "--adjustment"]),
        "ionice" => (&["-t", "--ignore"], &["-c", "--class", "-n", "--classdata"]),
        "stdbuf" => (&[], &["-i", "--input", "-o", "--output", "-e", "--error"]),
        "time" => (&["-p"], &[]),
        _ => (&[], &[]),
    };
    let short_stdbuf_value = wrapper == "stdbuf"
        && ["-i", "-o", "-e"]
            .iter()
            .any(|prefix| option.starts_with(prefix) && option.len() > prefix.len());
    if (flags.contains(&name) && !attached) || short_stdbuf_value {
        Some(0)
    } else if values.contains(&name) {
        Some(usize::from(!attached))
    } else {
        None
    }
}

fn is_shell_reentry(word: &str) -> bool {
    matches!(
        word.rsplit('/').next().unwrap_or_default(),
        "eval" | "source" | "." | "bash" | "sh" | "zsh" | "dash"
    )
}

/// Builtins that end the shell (or replace it, `exec`): nothing after them runs.
fn ends_the_shell(word: &str) -> bool {
    matches!(
        word.rsplit('/').next().unwrap_or_default(),
        "exec" | "exit" | "return" | "logout"
    )
}

fn is_wrapper(word: &str) -> bool {
    matches!(
        word.rsplit('/').next().unwrap_or_default(),
        "sudo"
            | "doas"
            | "env"
            | "exec"
            | "command"
            | "builtin"
            | "nohup"
            | "nice"
            | "ionice"
            | "setsid"
            | "stdbuf"
            | "time"
            | "timeout"
            | "xargs"
            | "parallel"
            | "su"
    )
}

fn is_variable_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

/// Commands that only show a file's contents or a directory's listing (issue #462): their
/// exit code proves no behaviour, so a run of them never verifies a `done`.
const READ_ONLY: [&str; 5] = ["cat", "sed", "head", "tail", "ls"];

/// True when every executable the command runs only reads or lists files ([`READ_ONLY`]),
/// a `cd` between them aside. An executable behind a supported wrapper (`env`, `timeout`, …)
/// is the one that counts, and an executable this lexer cannot locate is not read-only.
pub fn is_read_only(command: &str) -> bool {
    let shell = lex_shell(command);
    if shell.opaque {
        return false;
    }
    let mut reads = false;
    for segment in shell.tokens.split(|token| {
        matches!(token, ShellToken::Operator(operator) if matches!(operator.as_str(), "&&" | "||" | "|" | "&" | ";" | "\n"))
    }) {
        let Some(executable) = segment_executable(segment) else {
            return false;
        };
        // A bare name or a system path: `./ls` or `scripts/head` is the project's own program.
        let name = ["/usr/bin/", "/bin/"]
            .iter()
            .find_map(|prefix| executable.strip_prefix(prefix))
            .unwrap_or(executable);
        if READ_ONLY.contains(&name) {
            reads = true;
        } else if name != "cd" {
            return false;
        }
    }
    reads
}

/// The executable a segment runs: past leading assignments and supported wrappers, the way
/// [`segment_is_unprovable`] walks it; `None` when it cannot be located.
fn segment_executable(segment: &[ShellToken]) -> Option<&str> {
    let mut position = 0;
    while let Some(ShellToken::Word(word)) = segment.get(position) {
        if word.quoted || !is_variable_assignment(&word.text) {
            break;
        }
        position += 1;
    }
    loop {
        let Some(ShellToken::Word(word)) = segment.get(position) else {
            return None;
        };
        if word.expanded {
            return None;
        }
        if !is_wrapper(&word.text) {
            return Some(&word.text);
        }
        position = wrapper_command_position(&word.text, segment, position + 1)?;
    }
}

/// True when the command runs more than one command through `&&`: a non-zero exit code then
/// does not say which of them returned it.
pub fn is_chained(command: &str) -> bool {
    lex_shell(command)
        .tokens
        .iter()
        .any(|token| matches!(token, ShellToken::Operator(operator) if operator == "&&"))
}

/// True when the command contains an unquoted `|` outside `||`.
pub fn is_piped(command: &str) -> bool {
    lex_shell(command).tokens.iter().any(|token| {
        matches!(token, ShellToken::Operator(operator) if operator.contains('|') && operator != "||")
    })
}

/// Only adjacent `&&` and redirection operators preserve the check's exit status.
pub fn is_masked(command: &str) -> bool {
    lex_shell(command).tokens.iter().any(|token| {
        matches!(token, ShellToken::Operator(operator) if operator.contains([';', '\n'])
            || operator.contains("||")
            || (operator.contains('&') && !matches!(operator.as_str(), "&&" | ">&" | "<&" | "&>" | "&>>")))
    })
}
