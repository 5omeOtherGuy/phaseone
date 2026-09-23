# Design notes

The baseline for the first slice:

| File | What |
|---|---|
| `pillars.md` | What p1 is (rev 2) |
| `design-summary.md` | The design on one page |
| `seams.md` | Modules, contracts, acceptance criteria (§10), what carries over from Iris |

These are the joint working proposal that went into `DECISIONS.md`; owner decisions
there win over anything written here. Settled decisions are recorded as Architecture
Decision Records in `../adr/` (start at `../adr/README.md`); this directory holds the
design proposals they were made from. Notes in this directory record each shared seam
as it is actually built — a new shared seam gets a short note, small tasks do not.
When a proposal proves wrong, the note and `DECISIONS.md` are updated with evidence.

| Note | Seam |
|---|---|
| `routes.md` | Verified wire shapes of the two real provider routes |
| `tools.md` | Tool modules and the shared workspace helper |
| `context.md` | Context control: contract and the summarizing policy module |
| `completion.md` | Turn completion for unattended runs: finish tool, bounded continuation |
| `providers.md` | Provider modules, shared HTTP/SSE/retry helper, the ONE conformance suite |
| `assembly.md` | Environment files, catalog, fail-fast assembly, host CLI |
| `journal.md` | Memory/JSONL stores, sync guarantee, truncated tail, projection, resume, interrupted calls |
| `delegation.md` | Optional worker service and delegation tools |
| `core.md` | Agent core behaviour specification (authoritative for `p1-core`) |
| `research-program.md` | How research fans out: issue states, caps, curator limits, experiment design (ADR-0045) |
| `websocket.md` | WebSocket transport for the Responses adapter, SSE fallback, continuation (ADR-0047) |
| `model-selection.md` | Choosing, scoping and switching models: `--model`, `p1 models`, `settings.toml`, `/model` (ADR-0049) |
| `workflows.md` | Optional workflows: a sandboxed script orchestrates workers under roles and caps (ADR-0053) |
