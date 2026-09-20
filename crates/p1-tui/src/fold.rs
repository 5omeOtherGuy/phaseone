//! Fold blocks (SPEC §4.3): oversized tool output renders as a bounded head on
//! BLOCK background with a fold handle, instead of flooding the transcript.
//!
//! The handle id is stable and addressable — derived from the content, so the
//! same output always gets the same id and the right pane can open the same
//! object (`^O`). Donor concept: iris-agent ADR-0048 (oversized outputs behind
//! session-scoped handles), reimplemented at p1 scale.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Output of at most this many lines renders in full as a bounded block.
pub const FULL_BLOCK_MAX_LINES: usize = 40;
/// A folded block shows this many head lines above its handle line.
pub const FOLD_HEAD_LINES: usize = 8;

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
    /// Small enough to show whole, as a bounded block.
    Full { lines: Vec<String> },
    /// Over the full-block limit: head lines, then `· N more lines folded`.
    Folded {
        head: Vec<String>,
        folded: usize,
        id: FoldId,
    },
}

impl Fold {
    /// Decide the presentation of one tool output. `content` is EXACTLY what
    /// the model saw; the transcript never invents a shorter truth, it folds.
    pub fn present(content: &str) -> Self {
        let lines: Vec<String> = content.lines().map(str::to_string).collect();
        if lines.len() <= FULL_BLOCK_MAX_LINES {
            return Self::Full { lines };
        }
        let head = lines[..FOLD_HEAD_LINES].to_vec();
        Self::Folded {
            head,
            folded: lines.len() - FOLD_HEAD_LINES,
            id: FoldId::of(content),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_renders_in_full() {
        let fold = Fold::present("one\ntwo");
        assert_eq!(
            fold,
            Fold::Full {
                lines: vec!["one".into(), "two".into()]
            }
        );
    }

    #[test]
    fn large_output_folds_with_a_stable_handle() {
        let content: String = (0..89).map(|n| format!("line {n}\n")).collect();
        let fold = Fold::present(&content);
        let Fold::Folded { head, folded, id } = &fold else {
            panic!("expected a fold");
        };
        assert_eq!(head.len(), FOLD_HEAD_LINES);
        assert_eq!(*folded, 89 - FOLD_HEAD_LINES);
        // Stable: the same content hashes to the same handle.
        assert_eq!(*id, FoldId::of(&content));
        assert_eq!(id.0.len(), "h-0000".len());
    }

    #[test]
    fn the_boundary_is_exact() {
        let at_limit: String = (0..FULL_BLOCK_MAX_LINES)
            .map(|n| format!("{n}\n"))
            .collect();
        assert!(matches!(Fold::present(&at_limit), Fold::Full { .. }));
        let over: String = (0..=FULL_BLOCK_MAX_LINES)
            .map(|n| format!("{n}\n"))
            .collect();
        assert!(matches!(Fold::present(&over), Fold::Folded { .. }));
    }
}
