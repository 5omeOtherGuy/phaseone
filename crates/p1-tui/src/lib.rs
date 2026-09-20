//! The p1 terminal UI: a pure state machine and cell renderer.
//!
//! Everything here is `Screen` state in, `ratatui` cells out — no async, no TTY,
//! no agent. The async driver that owns the agent lives in the host
//! (`p1-host/src/tui.rs`) and talks to this crate through events and commands.
//! The visual vocabulary is fixed by `docs/design/tui/SPEC.md` §1–3 and §8;
//! those sections are not negotiable.

pub mod fold;
pub mod glyphs;
pub mod grid;
pub mod input;
pub mod palette;
pub mod render;
pub mod state;
pub mod transcript;
pub mod wrap;
