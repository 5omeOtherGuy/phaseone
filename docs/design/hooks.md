# Hooks

p1 ships no hook. The brain shadow hook of ADR-0058 (`p1-hook-shadow`, feature `shadow-hook`,
`[shadow]` in `settings.toml`) left the repository with ADR-0208: it served the owner's own
setup, not the product. The hook seam is the planned port of #711 (`docs/design/seams.md`,
family "hooks"); an observer of prompts or briefs comes back, if at all, on that port as a
module or an external part.
