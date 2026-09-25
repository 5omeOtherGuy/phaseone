//! System-clipboard writes for `/copy` and the OUTPUT pane's `y`. Adapted from the
//! read-only Iris donor (`iris-agent/src/ui/clipboard.rs`): prefer the platform's
//! clipboard tool (it keeps the selection after p1 exits), fall back to the OSC 52
//! terminal escape, and emit OSC 52 as well over SSH so the text lands on the
//! operator's machine. Copied text is never logged.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Terminals cap escape-sequence length; an oversized OSC 52 payload can
/// desynchronise rendering instead of failing cleanly (Iris/pi-mono cap).
const MAX_OSC52_ENCODED: usize = 100_000;

/// How a copy reached the clipboard, so the notice can be honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    /// A clipboard tool took the text: the system clipboard is set.
    Tool(&'static str),
    /// Only the OSC 52 escape was written: it lands if the terminal allows it.
    Osc52,
}

/// Copy `text`; `Err` names why nothing accepted it.
pub(crate) fn copy(text: &str, out: &mut dyn Write) -> Result<Method, String> {
    let remote = ["SSH_CONNECTION", "SSH_CLIENT", "MOSH_CONNECTION"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()));
    let tool = candidates(&Env::capture())
        .into_iter()
        .find(|argv| pipe(argv, text).is_ok())
        .map(|argv| argv[0]);
    if let Some(tool) = tool
        && !remote
    {
        return Ok(Method::Tool(tool));
    }
    match osc52(text) {
        Some(seq) => {
            out.write_all(seq.as_bytes())
                .and_then(|()| out.flush())
                .map_err(|e| e.to_string())?;
            Ok(tool.map_or(Method::Osc52, Method::Tool))
        }
        None => tool
            .map(Method::Tool)
            .ok_or_else(|| "too large for OSC 52 and no clipboard tool accepted it".into()),
    }
}

struct Env {
    wayland: bool,
    x11: bool,
    macos: bool,
}

impl Env {
    fn capture() -> Self {
        let set = |v: &str| std::env::var_os(v).is_some_and(|s| !s.is_empty());
        Self {
            wayland: set("WAYLAND_DISPLAY"),
            x11: set("DISPLAY"),
            macos: cfg!(target_os = "macos"),
        }
    }
}

fn candidates(env: &Env) -> Vec<&'static [&'static str]> {
    let mut out: Vec<&'static [&'static str]> = vec![];
    if env.macos {
        out.push(&["pbcopy"]);
    }
    if env.wayland {
        out.push(&["wl-copy"]);
    }
    if env.x11 {
        out.push(&["xclip", "-selection", "clipboard"]);
        out.push(&["xsel", "--clipboard", "--input"]);
    }
    out
}

/// Pipe `text` into a clipboard tool and wait (bounded) for it to exit. Its
/// stdout/stderr are null so a tool that forks to own the selection (wl-copy,
/// xclip) cannot hold the wait open.
fn pipe(argv: &[&str], text: &str) -> Result<(), String> {
    let mut child = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| e.to_string())?;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(format!("{} exited with {status}", argv[0])),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                return Err(format!("{} did not finish", argv[0]));
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn osc52(text: &str) -> Option<String> {
    let encoded = base64(text.as_bytes());
    (encoded.len() <= MAX_OSC52_ENCODED).then(|| format!("\x1b]52;c;{encoded}\x07"))
}

/// Standard base64 (RFC 4648, padded). Small enough not to be worth a crate.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The notice after a copy: what went where, never the text itself.
pub(crate) fn notice(lines: usize, bytes: usize, result: &Result<Method, String>) -> String {
    let what = format!(
        "{} · {}",
        p1_tui::render::block::plural(lines, "line"),
        p1_tui::render::block::size(bytes)
    );
    match result {
        Ok(Method::Tool(tool)) => format!("· copied {what} ({tool})"),
        Ok(Method::Osc52) => {
            let tmux = std::env::var_os("TMUX").is_some();
            format!(
                "· sent {what} via OSC 52 — needs terminal support{}",
                if tmux {
                    " (tmux: set -g set-clipboard on)"
                } else {
                    ""
                }
            )
        }
        Err(why) => format!("· copy failed: {why}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded);
        }
    }

    #[test]
    fn osc52_encodes_and_caps_the_payload() {
        assert_eq!(osc52("hi").unwrap(), "\x1b]52;c;aGk=\x07");
        assert!(osc52(&"x".repeat(80_000)).is_none());
    }

    #[test]
    fn candidates_follow_the_display_server() {
        let names = |env: Env| -> Vec<&str> { candidates(&env).iter().map(|a| a[0]).collect() };
        assert_eq!(
            names(Env {
                wayland: true,
                x11: true,
                macos: false
            }),
            ["wl-copy", "xclip", "xsel"]
        );
        assert!(
            names(Env {
                wayland: false,
                x11: false,
                macos: false
            })
            .is_empty()
        );
    }

    #[test]
    fn a_missing_or_failing_tool_is_an_error_not_a_hang() {
        assert!(pipe(&["p1-no-such-clipboard-tool"], "x").is_err());
        assert!(pipe(&["false"], "x").is_err());
        assert!(pipe(&["cat"], "x").is_ok());
    }
}
