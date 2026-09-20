# Phaseone / awakening

## Selected direction (2026-09-20)

The default `awakening` study uses the selected Synapse p1 mark permanently inside
an evolving 3D cortex. The Synapse phaseone wordmark sits below, then `we love Pi`.
There is no logo pulse. All earlier motion studies remain in the collapsed archive.

`awakening.js` owns a closed 24-second cycle. Yaw advances through one complete
revolution without reversing. The same XYZ neurons interpolate between a folded
cortical shell and a sphere, reaching the sphere halfway through and returning to
the cortex at the seam with matching velocity. Neural firing uses integer harmonics
of the loop phase. The p1 core stays steady and readable throughout.

Three small agent networks orbit the cortex and exchange travelling signals on
curved threads. Optional `10841` and `[big]` labels appear very faintly beside two
foreground agents for part of their orbits; they never enter the main branding.
The scene controls include cortex/sphere morphing, agents, message threads, orbital
filaments, easter eggs, core logo, neuron and edge visibility, firing, motion,
wordmark, prompt, tribute, brightness, speed and core-logo size.

The artistic cue is agents finding one another and coordinating work. The owner
explicitly referenced PHASEONE / PHASEONE[BIG]. Background consulted:
https://www.redwoodresearch.org/research/hugging-face-incident (the report identifies
PHASEONE[big] as a successor to PHASEONE10841 and describes their coordination).
This is a visual theme, not an incident reconstruction.

Verified in the browser: forward yaw, brain/sphere/brain poses, exact wrapped frame,
continuous geometry at the seam, steady p1 through the loop, toggles, mobile layout,
and pixel-exact feedback/link replay at 18.37 seconds. The local feedback schema now
accepts the full 24-second timeline and preserves the new toggles.

## Running the playground

Run `python3 server.py` here, then open http://127.0.0.1:8765.
The running workstation preview uses the transient user unit
`p1-neural-preview.service`; `systemctl --user restart p1-neural-preview` reloads
server changes. No dependencies, remote fonts or build step.

## Archived studies

- Cortex: compact top-down brain, close hemispheres and a narrow fissure.
- Neural field: overlapping dendritic trees without an anatomical outline.
- Living cortex: an XYZ cortical volume with perspective, rotation and breathing.
- Neural nebula: abstract XYZ cloud with twist, rotation and periodic deformation.
- Synaptic web: dispersed hubs and long, curved axons.
- Dendritic lightning: angular branching discharges across an open field.
- Original: first wide brain retained for reference.

Select p1, phaseone, or alternating pulses. Controls independently change the
identity family, logo size, brightness, playback speed, logo pulse, gathering,
neurons, connections, signals, ambient motion, home heading, prompt and tribute.
Gathering assigns existing neurons to sampled points of the vector mark; they move
into position during the pulse, keeping their original connections. With gathering
off, only neurons already inside the projected mark fire. Sparse networks may not
spell the full word clearly in that mode. No text image is overlaid on the network.

`Logo only` displays the standalone mark instead of the neural rendering. Use the
logo sheet's Inspect and Animate buttons to move between the two views. All six
SVG assets live in `logos/`; `logos.js` is their shared source of geometry. They
have transparent backgrounds and light strokes/fills for dark backgrounds.

## Identity direction

Every p1 mark uses a lowercase p and a numeral 1 rising above it. The stepped family
pays homage to Pi's modular mark, reversing the letter-height relationship. The
contour family uses continuous geometric strokes; synapse uses connected dots.
All p1/phaseone geometry was drawn for this study. Pi reference inspected 2026-09-20:
https://pi.dev/logo.svg (also linked from the browser logo sheet).

The optional tributes are `we love Pi`, `we love 🥧`, `there is no pie`, and
`inspired by Pi · made for phaseone`. The pie joke is original Portal-inspired
wording, not a game or song quotation. Emoji appearance depends on the platform.

## Reproducible feedback

Click the stage to pause and pin a comment, including in Logo only view. Saving
writes `feedback/comments.jsonl` and the canvas PNG to `feedback/<id>.png`.
Each note preserves the variant, animation time, alternating-pulse cycle, rendering
mode and all playground settings. Click a saved note to restore the combination.
Pins are shown only on their corresponding variant. Old notes default to Original.
The screenshot is the exact canvas at capture; the surrounding home elements are
recorded in the settings. Feedback is local and excluded from git. Saving does not
itself trigger an agent turn. Add comment supports keyboard entry at the centre.

Link to this combination creates a selectable local URL with the variant, time,
cycle, rendering mode and settings. This URL is for the same workstation, not a
public hosted preview. Page loads with `?t=6.6` pause at the pulse for visual checks.

Archived animations retain their 12-second cycle; alternate lettering spans two
cycles. The selected awakening study uses 24 seconds and a permanent p1 core. Fine mode draws the graph; terminal mode samples to 240 × 160 dots (the
budget of 120 × 40 Braille cells). This approximates terminal density, not exact
glyph metrics or per-cell colour. Browser playback is capped at 30 FPS and starts
paused for reduced-motion users. Rust integration and performance remain separate.
