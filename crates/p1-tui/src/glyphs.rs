//! The SPEC §2 symbol vocabulary: eight glyphs, each with exactly one job.
//! Every state must read with all colour stripped; colour only reinforces what
//! the glyph already says (tested in the snapshot suite).
//!
//! Donor pattern: iris-agent `src/ui/symbols.rs` (one closed vocabulary as
//! constants) — rewritten for p1's smaller set.

/// `›` — operator input.
pub const OPERATOR: char = '›';
/// `▸` — tool call.
pub const TOOL: char = '▸';
/// `✓` — completed.
pub const DONE: char = '✓';
/// `✗` — failed (INK on the marker, DIM on the detail).
pub const FAILED: char = '✗';
/// `!` — approval required.
pub const APPROVAL: char = '!';
/// `·` — queued / folded / skipped.
pub const PENDING: char = '·';
/// `↳` — delegate / nested.
pub const NESTED: char = '↳';
/// `▪` — working (LED chase).
pub const WORKING: char = '▪';

/// Working indicator (SPEC §2): three `▪` cells, 1.1 s cycle, 0.18 s stagger,
/// opacity 0.18 → 1.0. Not a braille spinner.
pub const WORKING_CELLS: usize = 3;
pub const WORKING_CYCLE_MS: u64 = 1100;
pub const WORKING_STAGGER_MS: u64 = 180;

/// The brightness of one working cell at `t` milliseconds into the cycle, in
/// 0.0..=1.0. Each cell runs a raised-cosine pulse delayed by `cell × stagger`;
/// a cell at rest sits at the 0.18 opacity floor. Pure over time so the
/// indicator is testable with fake time — no sleeps, ever.
pub fn working_opacity(cell: usize, t_ms: u64) -> f32 {
    const FLOOR: f32 = 0.18;
    let phase = (t_ms + WORKING_CYCLE_MS - cell as u64 * WORKING_STAGGER_MS) % WORKING_CYCLE_MS;
    // Pulse occupies the middle half of the cycle; the rest is the floor.
    let window = WORKING_CYCLE_MS / 2;
    if phase >= window {
        return FLOOR;
    }
    let x = phase as f32 / window as f32;
    FLOOR + (1.0 - FLOOR) * (x * std::f32::consts::PI).sin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_cells_stagger_and_rest_on_the_floor() {
        // Cell 0 peaks a quarter through its window; the neighbour peaks one
        // stagger later, and rests on the floor in between.
        let peak = WORKING_CYCLE_MS / 4;
        assert!(working_opacity(0, peak) > 0.9);
        let shifted = peak + WORKING_STAGGER_MS;
        assert!(working_opacity(1, shifted) > 0.9);
        assert!((working_opacity(1, 800) - 0.18).abs() < f32::EPSILON);
        // One full cycle later the same cell repeats exactly.
        assert_eq!(
            working_opacity(0, peak),
            working_opacity(0, peak + WORKING_CYCLE_MS)
        );
    }
}
