---
adr: 58
title: p1 spawns the brain shadow hook, detached and fail-open, as an optional module
status: accepted
date: 2026-09-23
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [docs/design/pillars.md, crates/p1-host/src/run.rs, crates/p1-host/src/models.rs, crates/p1-host/src/lib.rs]
---
# ADR-0058: p1 spawns the brain shadow hook, detached and fail-open, as an optional module

## Context

The owner's adaptive-memory programme (brain-tools) observes what agents are asked to do. Its
"shadow" computes a memory packet in its own process and writes one ledger row; it never sends
anything back. XO approved (owner proxy, 2026-09-23) a native p1 hook, ranked after the current
backend work and before the TUI. The brain-tools lead's verified spec is
`~/brain-tools-wt/l5a/docs/design/p1-shadow-hook.md` (branch `l5a/integrate`, 42749a9); the
p1 lead's conditions were folded into it: an optional crate composed in the host, off unless
configured, never a live spawn in the gate, fail-open and never awaited, a 0600 task file that
p1 never logs. p1's architecture rules apply unchanged: nothing in `p1-core`, no tool or
provider crate involved, compile-time composition with a constructor.

## Decision

1. **Crate `p1-hook-shadow`.** Owns everything the spec puts inside the module: the `STATE`
   directory rule (`$BRAIN_PACKET_STATE`, else `$HOME/.local/state/brain-packet`), the kill file
   (`STATE/kill` → nothing happens), the recursion guard (any of `BRAIN_PACKET_SHADOW`,
   `BRAIN_INTERNAL`, `BRAIN_JOB`, `BRAIN_HOME`, `BRAIN_PACKET_DEPTH` set, non-empty and not
   `0` → nothing), the task file (`STATE/inbox/p1-<pid>-<n>.txt`, `O_CREAT|O_EXCL`, mode 0600,
   `STATE/inbox` created 0700), the exact argv (`--harness p1 --origin hook --task-file …
   --workspace … --session S --episode E [--family …] [--provider …] [--source-ref …]`), the
   session key `S` (the journal's cache key, else `p1:` + 16 hex of sha256(journal path)) and
   episode `E` (the text's `Episode:` line, else `p1-`/`p1-agent-` + 12 hex of sha256(text)),
   and the detach recipe (`Command … process_group(0).spawn()`, `Child` handed to a reaper
   thread, stdio null, nothing read). It depends on the standard library only.
2. **Constructor and interface.** `ShadowHook::new(binary: PathBuf, env: &dyn Fn(&str) ->
   Option<OsString>) -> ShadowHook`; `fn observe(&self, event: ShadowEvent)` with
   `ShadowEvent { text: String, workspace: Option<PathBuf>, journal: PathBuf, cache_key:
   Option<String>, origin: Origin::UserInput | Origin::Dispatch { family: String, provider:
   String }, source_ref: Option<String> }`. `observe` returns nothing and never fails
   visibly; a spawn failure removes only p1's own task file. A `Locations`-style env accessor
   is injected so tests never touch the real environment.
3. **Host composition, behind cargo feature `shadow-hook` (default on).** The host builds the
   hook only when `settings.toml` has `[shadow] brain_packet_shadow = "/abs/path"` or
   `brain-packet-shadow` is found on `PATH`; otherwise `None` and no code path runs. `HostDeps`
   carries `Option<Arc<ShadowHook>>`. Two call sites: every `UserInput` record the host commits
   (interactive or headless), and every worker dispatch (the child factory's task, and
   therefore every workflow step). Calls are made after the journal record is committed and
   never delay it.
4. **Tests never spawn the real binary.** p1's gate runs with the hook unset, or against a
   fake binary (a script appending its argv to a file). Acceptance (a) unit: kill file → no
   spawn and no file; each recursion variable → no spawn; normal → one spawn with the exact
   argv and a 0600 task file in `STATE/inbox`; nonexistent binary → no error surfaced, task
   file removed. (b) 20 hook calls ≤ 50 ms p95 (measured and printed, asserted loosely).
   (c) One live check by the lead with the spec's recipe: scratch `STATE`, p1's own HOME
   untouched, brain engine path `/nonexistent`, ≥ 1 ledger row with `harness: p1`, and the
   session journal byte-identical with the feature off and on.
5. **Privacy.** The prompt text goes only into the 0600 task file the spec owns; p1 never logs
   or journals it a second time, never reads the ledger, and the run report says only that
   the hook is mounted.

## Consequences

- The brain sees p1 prompts and dispatches without a journal watcher; p1 behaves byte-for-byte
  as before, ≤ 50 ms per hook call on the host thread.
- A machine with `brain-packet-shadow` on `PATH` gets the hook automatically; the kill file is
  the operator's off switch, a settings key the explicit one.
- One more optional crate; nothing in the core; the delegation and workflow modules are not
  touched, only the host's two call sites.

## Alternatives considered

- A journal watcher on the brain side: what this replaces (latency, and it misses dispatches).
- Reading the hook's result back into the session: rejected by the spec ("never sends a byte
  back") and by p1's rule that an agent sees only its assembled prompt and tools.

## Evidence

Merged as cbcaf17's parent merge of task/shadow-hook (gpt-6-sol worker, reviewed by the lead;
gate green; run recorded in `docs/dogfood/runs.jsonl`). Unit tests in `crates/p1-hook-shadow/
tests/hook.rs`: kill file → no file, no spawn; each of the five recursion variables → no spawn
(and `0`/empty do not guard); a dispatch and a user input → one spawn each with the exact argv
and a private (0600) task file holding the exact bytes; explicit `Episode:` and derived session
key honoured; nonexistent binary → only the new task file removed; HOME fallback and `PATH`
discovery use only the injected environment; 20 calls with the p95 printed. Host tests
(`crates/p1-host/tests/shadow_hook.rs`): typed `[shadow]` settings rejecting unknown keys; a
committed parent input is observed; a worker dispatch is observed after its own commit. Live,
2026-09-23, the spec's recipe: scratch STATE and HOME, brain engine `/nonexistent`, a wrapper
around `brain-tools-wt/l5a/scripts/brain-packet-shadow`, `[shadow] brain_packet_shadow` naming
it, one prompt on gpt-6-sol with the feature on and one with it off (PATH without `~/.local/bin`):
both exit 0, the ledger holds one `harness: p1` row, `STATE/inbox` is empty afterwards (the
shadow took the task file), and the two session journals' environment and user-input records
are byte-identical.
