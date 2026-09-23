# Shadow hook (ADR-0058)

With the default `shadow-hook` feature, p1 optionally mounts `p1-hook-shadow` when
`[shadow] brain_packet_shadow = "/absolute/path/to/brain-packet-shadow"` is set
in `settings.toml`, or when `brain-packet-shadow` is found on `PATH`.
The hook fires after each real user input and initial worker brief is committed
to the session journal. It writes a private task file and launches the binary
detached, without reading output or waiting for a result. p1 never reads anything
back. To disable it without changing settings, create `$BRAIN_PACKET_STATE/kill`
(or `$HOME/.local/state/brain-packet/kill` when that variable is unset).
