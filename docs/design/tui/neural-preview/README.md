# Neural home motion study

Run `python3 server.py` from this directory, then open http://127.0.0.1:8765.
No dependencies, remote fonts, build step or network services.

Click the brain to pause and pin a comment. Save writes a note to
`feedback/comments.jsonl` and the canvas PNG to `feedback/<id>.png`.
The agent can read those files directly; saving does not itself trigger an agent turn.
Click a saved note to revisit its exact time and rendering mode. Feedback is local
and excluded from git. The Add comment button supports keyboard entry at the centre.

Animation is a deterministic 12-second cycle with a fixed seed. Use `?t=6.6` for
a paused logo frame or `?t=2&mode=terminal` for a paused terminal approximation.
The logo is a provisional geometric lowercase p1, encoded as neuron membership.
Fine mode draws the underlying graph; terminal mode samples it to a 240 × 160
dot field, equivalent to the dot budget of 120 × 40 Braille cells. Terminal glyphs,
per-cell colour and final allocation within the home screen still need a Rust pass.
The existing monochrome palette is preserved. Reduced-motion preference starts paused.

This is an art prototype; it does not change the Rust TUI or implement its composer.
