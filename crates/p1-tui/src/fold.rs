//! Fold blocks (BLOCK-SPEC §5): an output over 40 body rows renders as a
//! bounded 24-row window on BLOCK background with a fold handle, instead of
//! flooding the transcript. Below 40 rows nothing folds — expanded is the
//! default, so the operator never has to press a key to see a result they are
//! being asked to judge.
//!
//! The handle id is stable and addressable — derived from the content, so the
//! same output always gets the same id and the right pane can open the same
//! object (`^O`). Donor concept: iris-agent ADR-0048 (oversized outputs behind
//! session-scoped handles), reimplemented at p1 scale.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Bodies of at most this many rows render in full. Over it, they fold.
pub const FOLD_THRESHOLD: usize = 40;
/// A folded body keeps this many rows, from the head or the tail.
pub const FOLD_KEEP_LINES: usize = 24;

/// Which end of an oversized body survives the fold. A shell run's verdict is
/// at the bottom; a read/edit's head is what the operator asked to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldKeep {
    Head,
    Tail,
}

impl FoldKeep {
    /// The fold direction for a tool (BLOCK-SPEC §5): `shell` keeps its tail,
    /// everything else keeps its head.
    pub fn for_tool(name: &str) -> Self {
        match name {
            "shell" => FoldKeep::Tail,
            _ => FoldKeep::Head,
        }
    }
}

/// A stable fold handle, e.g. `h-7c21`. Stable because it is a content hash:
/// reopening the same output after a resume finds the same handle.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FoldId(pub String);

impl FoldId {
    pub fn of(content: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        Self(format!("h-{:04x}", hasher.finish() as u16))
    }
}

impl std::fmt::Display for FoldId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How one tool output is shown in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fold {
    /// At or below the threshold: every row shows, as a bounded block.
    Full { lines: Vec<String> },
    /// Over the threshold: `kept` rows, then `· N more lines folded`.
    Folded {
        kept: Vec<String>,
        keep: FoldKeep,
        folded: usize,
        id: FoldId,
    },
}

impl Fold {
    /// Decide the presentation of one tool output. `content` is EXACTLY what
    /// the model saw; the transcript never invents a shorter truth, it folds.
    pub fn present(content: &str, keep: FoldKeep) -> Self {
        let lines: Vec<String> = content.lines().map(str::to_string).collect();
        if lines.len() <= FOLD_THRESHOLD {
            return Self::Full { lines };
        }
        let kept = match keep {
            FoldKeep::Head => lines[..FOLD_KEEP_LINES].to_vec(),
            FoldKeep::Tail => lines[lines.len() - FOLD_KEEP_LINES..].to_vec(),
        };
        Self::Folded {
            kept,
            keep,
            folded: lines.len() - FOLD_KEEP_LINES,
            id: FoldId::of(content),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_renders_in_full() {
        let fold = Fold::present("one\ntwo", FoldKeep::Head);
        assert_eq!(
            fold,
            Fold::Full {
                lines: vec!["one".into(), "two".into()]
            }
        );
    }

    #[test]
    fn a_head_fold_keeps_the_top_and_a_tail_fold_the_bottom() {
        let content: String = (0..89).map(|n| format!("line {n}\n")).collect();
        let Fold::Folded {
            kept, folded, id, ..
        } = Fold::present(&content, FoldKeep::Head)
        else {
            panic!("expected a fold");
        };
        assert_eq!(kept.len(), FOLD_KEEP_LINES);
        assert_eq!(kept[0], "line 0");
        assert_eq!(kept[FOLD_KEEP_LINES - 1], "line 23");
        assert_eq!(folded, 89 - FOLD_KEEP_LINES);
        assert_eq!(id, FoldId::of(&content));

        let Fold::Folded { kept, .. } = Fold::present(&content, FoldKeep::Tail) else {
            panic!("expected a fold");
        };
        assert_eq!(kept[0], "line 65");
        assert_eq!(kept[FOLD_KEEP_LINES - 1], "line 88");
    }

    #[test]
    fn the_threshold_is_exact_and_nothing_folds_below_it() {
        let at_limit: String = (0..FOLD_THRESHOLD).map(|n| format!("{n}\n")).collect();
        assert!(matches!(
            Fold::present(&at_limit, FoldKeep::Head),
            Fold::Full { .. }
        ));
        let over: String = (0..=FOLD_THRESHOLD).map(|n| format!("{n}\n")).collect();
        assert!(matches!(
            Fold::present(&over, FoldKeep::Head),
            Fold::Folded { .. }
        ));
    }

    #[test]
    fn shell_folds_from_the_tail_and_everything_else_from_the_head() {
        assert_eq!(FoldKeep::for_tool("shell"), FoldKeep::Tail);
        assert_eq!(FoldKeep::for_tool("read"), FoldKeep::Head);
        assert_eq!(FoldKeep::for_tool("edit"), FoldKeep::Head);
        assert_eq!(FoldKeep::for_tool("anything_else"), FoldKeep::Head);
    }
}
