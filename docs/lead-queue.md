# p1 lead responsibilities and queue

The p1 lead (the existing Opus 5.5 lead session) owns p1 by owner order of 2026-09-24 21:14.
It commands the dashboard lead (unified-dashboard programme: #89, #92, #111, observability terminal views) and the Iris lead (TUI port, `docs/iris-workflow.md`).
Since 2026-09-24 ~23:45 it also owns the retired keys-worker's estate: the per-account Go, Zen and Cline Pass routes, the key register (names only) and keys-sync.
The dashboard lead's brief is `unified-dashboard-trial/astra-lead-brief.md` despite its historical filename.
Follow project and global landing rules.
Read the board, current branches, CI and binary hashes before treating handoff status as current.
Keep `STATUS.md` current and post board state when it changes.

## Work order

1. Deliver `docs/worker-observability.md`; earlier analysis is under `~/projects/phaseone-briefs/worker-observability/`.
2. Keep the free routes working: #107 shipped Go-1/3, Zen-1/2/3 and Cline Pass-1/2; MiMo/Muse wait for the Zen client-identity fix; OpenRouter free tier and account rotation follow the free-quota plan (#117, #118).
Keys enter a p1 store only as `p1 login <route> < ~/.config/keys/<name>.key` (keys-sync does this; check with `keys-sync --check`); never expose values to agent context.
Validate actual tool use, identity, usage/errors and route receipt before moving workflow roles to p1.
Coordinate route writers and research evidence; do not duplicate bootstrap work.
3. Finish sanitized provider-error classification: plan refusals (#101) and quota 429s (#105) have landed; a Kimi 403 “quota exhausted” must still not appear as a rejected key; include reset hints.
Consult `credential-routes/STATUS.md`.
4. Implement native p1 Claude/Codex grants under `docs/design/credentials.md` §8.4.
For now use the owner-approved Claude trial login chain while keeping shipped ADR-0061 store-only behavior.
Preserve request-time loading, near-expiry refresh, one forced refresh on 401, refresh coalescing and atomic write-back that never overwrites newer credentials.
Use minimalcc-pi as the implementation reference; do not inspect credential values.
5. Complete `p1 usage` for Go `/zen/go/v1/usage`, Kimi `coding/v1/usages` and Z.ai quota.
Read `~/projects/phaseone-briefs/usage-dashboard-tmux.md`; inspect any reusable work in `~/projects/phaseone-usage-dashboard-tmux` before creating a replacement.
6. Reconcile ADR-0060 with the current build-storage order.
7. Fix and issue-track fanout/p1 defects encountered throughout the programme.
8. Context window by role (#113): leads 500k, workers 300k, capped by each route's capacity.
9. Build p1 on the GitHub Actions farm: push a `task/**` branch and run `scripts/ci-build.sh` (green = the run of exactly that commit, artifact in `ci-artifacts/<sha>/`); local cargo is `cargo check -p <crate>` plus the lead's deployed-binary rebuild only.

Keep interface, gate and rollback decisions in their project records.
Use `docs/iris-workflow.md` for coordination with Iris and ownership of shared UI seams.
Historical handoff, including old commit/binary observations: `~/.agents/archive/P1-LEAD-DUTIES-20260924-history.md`.
