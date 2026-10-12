//! The Codex face of `apply_patch` (ADR-0219): the description, the Lark grammar and the
//! result text of the Codex CLI, byte for byte, over the patch logic p1's own `apply_patch`
//! runs (`p1-tool-patch-logic`).
//!
//! Copied from openai/codex at tag `rust-v0.162.1` (commit 092d3acd), Apache-2.0
//! (`docs/licenses/openai-codex/`): `codex-rs/core/src/tools/handlers/apply_patch_spec.rs`,
//! `core/assets/tools/apply_patch.lark`, `apply-patch/src/lib.rs` (`print_summary`),
//! `core/src/tools/mod.rs` (`format_exec_output_for_model`) and
//! `core/src/tools/handlers/apply_patch.rs` (the verification failure).
//!
//! The patch semantics are p1's: the same hunks, the same all-or-nothing apply, the same
//! failure messages, which this face wraps as Codex wraps a verification failure.
#![forbid(unsafe_code)]

pub use p1_tool_patch_logic::GRAMMAR_SYNTAX;

pub const NAME: &str = "apply_patch";
pub const DESCRIPTION: &str = "The `apply_patch` tool can be used to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.";

/// `apply_patch.lark` as Codex ships it. p1's own copy (`p1_tool_patch_logic::PATCH_GRAMMAR`)
/// is the same grammar without the blank lines; the model is sent these bytes.
pub const GRAMMAR: &str = r#"start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?

filename: /(.+)/
add_line: "+" /(.*)/ LF -> line

change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF

%import common.LF
"#;

/// A successful apply as Codex shows it: its exec framing around `print_summary`. `lines`
/// are p1's success lines (`A path`, `M path`, `D path`, one per operation); a move is the
/// `M` of its source path, as Codex records a moved file.
pub fn success(lines: &str, wall_time_ms: u64) -> String {
    let duration_seconds = ((wall_time_ms as f32 / 1_000.0) * 10.0).round() / 10.0;
    let mut summary = String::from("Success. Updated the following files:\n");
    for line in lines.lines() {
        let line = match line.split_once(" -> ") {
            Some((moved, _)) => moved,
            None => line,
        };
        summary.push_str(line);
        summary.push('\n');
    }
    format!("Exit code: 0\nWall time: {duration_seconds} seconds\nOutput:\n{summary}")
}

/// A refused patch as Codex words it: the verification failure, with p1's own reason.
pub fn verification_failed(reason: &str) -> String {
    format!("apply_patch verification failed: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grammar_is_codex_s_byte_for_byte_and_p1_s_without_blank_lines() {
        let squeezed = GRAMMAR
            .lines()
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert_eq!(squeezed, p1_tool_patch_logic::PATCH_GRAMMAR);
        assert!(GRAMMAR.starts_with("start: begin_patch hunk+ end_patch\n"));
        assert!(GRAMMAR.ends_with("%import common.LF\n"));
    }

    #[test]
    fn success_is_the_exec_framing_around_print_summary() {
        assert_eq!(
            success(
                "A new.txt\nM changed.txt\nM old.rs -> new.rs\nD gone.txt",
                40
            ),
            "Exit code: 0\nWall time: 0 seconds\nOutput:\nSuccess. Updated the following files:\nA new.txt\nM changed.txt\nM old.rs\nD gone.txt\n"
        );
        assert_eq!(
            success("M a", 1_250),
            "Exit code: 0\nWall time: 1.3 seconds\nOutput:\nSuccess. Updated the following files:\nM a\n"
        );
        assert_eq!(
            verification_failed("hunk 1 of a.txt does not match"),
            "apply_patch verification failed: hunk 1 of a.txt does not match"
        );
    }
}
