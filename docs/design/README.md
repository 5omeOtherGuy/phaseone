# Design notes

The baseline for the first slice:

| File | What |
|---|---|
| `pillars.md` | What p1 is (rev 2) |
| `design-summary.md` | The design on one page |
| `seams.md` | Modules, contracts, acceptance criteria (§10), what carries over from Iris |

These are the joint working proposal that went into `DECISIONS.md`; owner decisions
there win over anything written here. Notes in this directory record each shared seam
as it is actually built — a new shared seam gets a short note, small tasks do not.
When a proposal proves wrong, the note and `DECISIONS.md` are updated with evidence.

| Note | Seam |
|---|---|
| `routes.md` | Verified wire shapes of the two real provider routes |
| `tools.md` | Tool modules and the shared workspace helper |
| `providers.md` | Provider modules, shared HTTP/SSE/retry helper, the ONE conformance suite |
| `assembly.md` | Environment files, catalog, fail-fast assembly, host CLI |
| `core.md` | Agent core behaviour specification (authoritative for `p1-core`) |
