# Research #42 — what to take from iris-agent (used: shell output filters; rest discarded)

Owner directive 2026-09-21. Decision memo and leaf report: `../phaseone-briefs/research/42/`.
Donors read-only: `iris-agent@62c8345`, `iris-agent-clean@5b04a1a`.

## Premise

There is no component called "token optimizer" in iris. What exists is a family of per-tool
output reducers (iris ADR-0036/0037); the largest is the bash output filter.

## Taken

**Structured shell-output filters** → `crates/p1-tool-shell/src/filter/` (merged `e1229fe`;
donor paths in that commit message): cargo test/build/check/clippy, git status/log/diff,
npm/pnpm test; `raw: true` opts out; any doubt yields the raw output; the exit-code footer is
never touched. iris measured, on its own 17-file corpus at 4 bytes/token: cargo test (pass)
439→26, cargo build (pass) 62→1, git log 792→303.

## Measured on p1 (2026-09-21, aggregate over 22 dogfood journals, 20.1 MB of tool results)

| Share of all tool-result bytes | |
|---|---|
| shell output, all commands | 22.5 % |
| recognised commands, run unpiped — what the filter can act on | **2.2 %** (131 of 1,502 shell calls) |
| recognised commands the model had already piped through `tail`/`grep` | 2.5 % |
| everything else — overwhelmingly `read` and `grep` | 77.5 % |

So on p1's traffic the filter's ceiling is about 2 % of tool-result bytes. It stays: it is
fail-safe, and it removes the reason models pipe their checks through `tail` — a pipe `finish`
rejects as verification. It is NOT a cost lever, and no saving is claimed for it.

## Not taken

- **The declarative TOML filter engine** and its 64 filter files: a process-global registry, a
  build script (p1 has none), and Apache-2.0 third-party data (`rtk-ai/rtk`) in an MIT repository.
- **`read` skim** (`iris: src/tools/skim.rs`): discarded for now. It needs the model to ask for
  it, and p1's `edit` needs the exact text of a file — a skimmed read cannot feed an edit.
  The measurement says file reading IS where the bytes are, but the lever there is fewer
  re-reads (run split4a: 698 reads, no edit), not thinner reads. Reopen with: evidence that
  models read for orientation at scale and would use a skim, or a design in which a skimmed read
  can never be mistaken for the file's text.
- Retry, transports, diff/patch handling, cost accounting: p1 is equal or better
  (leaf-reported, spot-checked). WebSocket is research #41.
