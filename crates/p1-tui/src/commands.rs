//! The TUI's local slash commands: one registry for parsing, `/help` and the
//! palette. A command is a single line whose first word names a registered
//! command (Iris `slash.rs` rule); anything else — a path like `/usr/bin`, a
//! pasted block, prose after a no-argument command — is a prompt for the agent.

/// What a command accepts after its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Args {
    None,
    Optional(&'static str),
    Required(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    pub args: Args,
    pub description: &'static str,
}

const fn cmd(name: &'static str, args: Args, description: &'static str) -> Command {
    Command {
        name,
        args,
        description,
    }
}

/// Every local command, in palette order.
pub const COMMANDS: &[Command] = &[
    cmd("help", Args::None, "keys and commands"),
    cmd("outputs", Args::None, "pick a retained output to open"),
    cmd(
        "open",
        Args::Optional("h-xxxx"),
        "open an output in the pane",
    ),
    cmd("close", Args::None, "close the output pane"),
    cmd(
        "find",
        Args::Required("text"),
        "search the transcript; again for older",
    ),
    cmd("filter", Args::Required("text"), "filter the open output"),
    cmd(
        "copy",
        Args::Optional("output|h-xxxx"),
        "copy the last reply or an output",
    ),
    cmd(
        "goal",
        Args::Optional("text"),
        "set or clear the session goal",
    ),
    cmd("status", Args::None, "environment, route and spend"),
    cmd(
        "focus",
        Args::Optional("on|off"),
        "fold passive chrome away",
    ),
    cmd(
        "pane",
        Args::Optional("ledger|output|diff|workers"),
        "switch the right pane",
    ),
    cmd(
        "mouse",
        Args::None,
        "release the mouse for terminal selection",
    ),
    cmd("exit", Args::None, "quit p1"),
    cmd("quit", Args::None, "quit p1"),
];

/// How a submitted line reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed<'a> {
    /// A registered command and its (trimmed) argument text.
    Command(&'static Command, &'a str),
    /// `/word` alone that names no command: worth a note, not a model turn.
    Unknown(&'a str),
    /// Anything else goes to the agent; `//x` sends `/x` literally.
    Prompt,
}

pub fn parse(text: &str) -> Parsed<'_> {
    let text = text.trim();
    let Some(rest) = text.strip_prefix('/') else {
        return Parsed::Prompt;
    };
    if rest.starts_with('/') || text.contains('\n') {
        return Parsed::Prompt;
    }
    let (name, arg) = rest.split_once(' ').unwrap_or((rest, ""));
    let arg = arg.trim();
    // A path-like first word (`/usr/bin`, `/etc/hosts.`) is prose, not a command.
    if name.is_empty() || name.contains(['/', '.', '\\']) {
        return Parsed::Prompt;
    }
    match COMMANDS.iter().find(|c| c.name.eq_ignore_ascii_case(name)) {
        Some(command) if command.args == Args::None && !arg.is_empty() => Parsed::Prompt,
        Some(command) => Parsed::Command(command, arg),
        None if arg.is_empty() => Parsed::Unknown(name),
        None => Parsed::Prompt,
    }
}

/// The line to send when `text` is a prompt: `//x` loses one slash. Only a
/// one-line `//word` is the escape — a `// comment` or pasted code that starts
/// with `//` reaches the model byte for byte.
pub fn prompt_text(text: &str) -> String {
    match text.trim_start().strip_prefix("//") {
        Some(rest) if !text.contains('\n') && rest.starts_with(|c: char| c.is_alphanumeric()) => {
            format!("/{rest}")
        }
        _ => text.to_owned(),
    }
}

/// Commands whose name starts with what is typed after `/` — the palette's rows.
/// Only while the draft is one line, starts with `/` and has no argument yet.
pub fn matches(text: &str) -> Vec<&'static Command> {
    let Some(rest) = text.strip_prefix('/') else {
        return vec![];
    };
    if text.contains('\n') || rest.contains(' ') || rest.starts_with('/') {
        return vec![];
    }
    COMMANDS
        .iter()
        .filter(|c| c.name.len() >= rest.len() && c.name[..rest.len()].eq_ignore_ascii_case(rest))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_single_line_registered_commands_are_commands() {
        assert!(matches!(parse("/help"), Parsed::Command(c, "") if c.name == "help"));
        assert!(
            matches!(parse("/Find some text"), Parsed::Command(c, "some text") if c.name == "find")
        );
        assert_eq!(parse("/usr/bin/env python3 fails"), Parsed::Prompt);
        assert_eq!(parse("/etc/hosts has a line"), Parsed::Prompt);
        assert_eq!(parse("/help me with this bug"), Parsed::Prompt);
        assert_eq!(parse("/status\nsecond line"), Parsed::Prompt);
        assert_eq!(parse("//help"), Parsed::Prompt);
        assert_eq!(prompt_text("//help"), "/help");
        assert_eq!(prompt_text("// TODO: keep"), "// TODO: keep");
        assert_eq!(prompt_text("//x\nfn main() {}"), "//x\nfn main() {}");
        assert_eq!(parse("/bogus"), Parsed::Unknown("bogus"));
        assert_eq!(parse("/tmp is full"), Parsed::Prompt);
        assert_eq!(parse("fix it"), Parsed::Prompt);
    }

    #[test]
    fn the_palette_matches_by_prefix_until_an_argument_starts() {
        let names = |t| matches(t).iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(names("/f"), ["find", "filter", "focus"]);
        assert_eq!(
            names("/"),
            COMMANDS.iter().map(|c| c.name).collect::<Vec<_>>()
        );
        assert!(matches("/find x").is_empty());
        assert!(matches("hello").is_empty());
        assert!(matches("/a\nb").is_empty());
    }
}
