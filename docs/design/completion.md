# Turn completion for unattended runs — specification

Owner failure F1: the agent stops although the task authorizes it to go on — it ends its turn
with a plan, a progress note, or "shall I continue?". This policy addresses exactly that, and
only that (review 2026-09-20, plan amendment 5). It does NOT address invented constraints or
forgotten decisions (context control and prompts do), and it is not a "never stop" rule: a
rule like that turns into a loop repeating failed actions.

The design makes completion an OBSERVABLE ACT instead of a phrase to be detected:
the model ends its work by calling a tool. No text is ever pattern-matched.

## 1. Definitions

- **Verified completion** — the model calls `finish` with `status: "done"` and names the
  verification commands it ran; and the session's own record shows that each of them really
  ran, succeeded, and ran AFTER the last file change. The tool checks this; the model's word
  is not enough.
- **Real blocker** — the model calls `finish` with `status: "blocked"`: what it needs, and what
  it tried. The run stops and reports it. A blocker is never answered with "continue".
- **Premature stop** — a turn that ends (`TurnEnd::Completed`) in an unattended run while no
  `finish` call has been ACCEPTED. Only this is continued.
- **Justified continuation** — after a premature stop, and only while the bounds in §3 hold.

## 2. The `finish` tool — crate `p1-tool-finish` (effect `ReadOnly`)

`{"status": "done"|"blocked", "summary": string, "verification"?: [string], "needs"?: string, "tried"?: [string]}`
(`deny_unknown_fields`; common input rules of tools.md).

The tool reads the session through a small trait it owns, implemented by the host:
```rust
pub struct ShellRun { pub command: String, pub exit_code: Option<i32>, pub order: u64 }
pub trait SessionActivity: Send + Sync {
    /// `order` of the last finished tool call whose effect was `WritesFiles`, if any.
    fn last_file_change(&self) -> Option<u64>;
    /// Every finished `Executes` call so far, oldest first.
    fn shell_runs(&self) -> Vec<ShellRun>;
}
pub struct FinishTool; impl FinishTool { pub fn new(activity: Arc<dyn SessionActivity>, outcome: FinishOutcome) -> Self; }
#[derive(Clone, Default)] pub struct FinishOutcome;   // shared cell the host reads
impl FinishOutcome { pub fn get(&self) -> Option<Accepted>; pub fn clear(&self); }
pub enum Accepted { Done { summary: String }, Blocked { summary: String, needs: String, tried: Vec<String> } }
```
Rules, each with its exact model-visible text:
1. `done` with an empty or missing `verification` →
   Error `Name the commands you ran to verify the work in "verification". If nothing can be verified by a command, say why in "summary" and pass ["none"].`
   `["none"]` is accepted ONLY when the session has no file change at all (pure question/answer
   work); otherwise → Error `This session changed files; verify the result with a command before finishing.`
2. Each named command must equal (after trimming) the `command` of a recorded shell run with
   exit code 0 → otherwise Error `No successful run of \`<command>\` is recorded in this session. Run it, read the result, then finish.`
   (the LAST run of that command counts: a failing re-run invalidates an earlier success).
3. That run must be newer than the last file change → otherwise
   Error `You changed files after running \`<command>\`. Run it again, then finish.`
   A shell command is itself not counted as a file change (it cannot be known); stated limit.
4. `blocked` requires non-empty `needs` → otherwise Error `Say what you need in "needs".`
5. Accepted `done` → Ok `Finished.`; accepted `blocked` → Ok `Recorded as blocked.` The outcome
   is stored in `FinishOutcome` (last accepted call wins). A rejected call stores nothing.
An Error is an ordinary tool result: the model reads it and keeps working in the same turn.

**Revision after dogfood run 3 (2026-09-20).** A real model needed TEN `finish` calls: seven
omitted `verification` although the error asked for it, two named a command in a different
spelling than it had run (`cargo fmt --check` vs `cd <dir> && cargo fmt --check`). And the
journal showed a hole: `cargo test … | tail -5` exits 0 even when the tests fail. Therefore:
- **Matching is normalised**, on both sides: trim, collapse runs of whitespace, and drop ONE
  leading `cd <path> &&` segment. Equality after that.
- **A pipeline is not a verification.** A recorded command containing an unquoted `|` that is
  not part of `||` never counts (its exit code is the last command's). Naming such a run →
  Error `\`<command>\` was run through a pipe, so its exit code says nothing about it. Run it without a pipe, then finish.`
  (stated limit: quoting is judged by a simple scan for `'…'` and `"…"`, not a shell parser).
- **Every rejection shows what WOULD be accepted.** Errors 1–3 end with a blank line and
  `Runs that count right now (successful, not piped, after the last file change):` followed by
  up to 5 normalised commands, newest last, one per line prefixed `- `; or
  `No run counts right now: run your checks (without a pipe) after your last file change.`
  Error 1 additionally shows the shape:
  `Call finish again with "verification": ["<one of the commands below>"].`
- The tool description says the same in two sentences (no pipe; name the command as you ran it).

## 3. Host policy (headless runs only)

Active when the assembled environment contains the `finish` tool AND the run is headless;
the interactive prompt never continues on its own (the user is there). After every turn end:
- `TurnEnd` other than `Completed` → as today.
- `FinishOutcome` holds `Done` → exit 0. `Blocked` → print `blocked: <needs>` (and the tried
  list) to stderr, exit `EXIT_BLOCKED = 3`.
- Otherwise, FIRST the things a turn end legitimately waits for: pending inbox messages → run an
  inbox turn; else workers still running → wait for the inbox (cancellable), then run the
  inbox turn. A parent that ended its turn while its worker runs is WAITING, not stopping —
  the worker's completion notification wakes it, exactly as without `finish`. Each such turn
  end is judged again from the top of this list.
- Only with an empty inbox and no running worker is it a premature stop. If BOTH bounds allow, send ONE user-role message and run
  another turn; else print `stalled: the agent stopped <n> times without finishing` and exit
  `EXIT_STALLED = 4`:
  - at most `max_continuations` in the whole run (default 3; `--max-continuations N`, 0 disables);
  - at most 1 continuation in a row without PROGRESS, where progress = at least one tool call
    other than `finish` finished since the previous continuation.
  The message, exactly:
  `You ended your turn without calling finish. You are running unattended: nobody will answer a question or confirm a plan, and this task authorizes you to continue on your own. Continue the work now. When it is complete and verified, call finish with status "done"; if something outside your control stops you, call finish with status "blocked".`
  It is committed as a normal `UserInput` record, so the journal shows every continuation.
- Workers: a child agent's environment may contain `finish` too; the worker service is
  unchanged in this increment (a child's turn end is its completion, the parent verifies).
The host implements `SessionActivity` from the event stream it already receives
(`ToolStarted`/`ToolFinished`), looking up each call's `effect` on the assembled tool and
parsing the shell footer `[exit code: N]`. No new core or contract surface. On `--resume` the
log is REBUILT from the journal's `ToolStarted`/`ToolFinished` records, so a verification run
before the restart still counts and a file change before it still invalidates.

## 3b. Provider failures in a headless run (issue #11)

A turn that ends `ProviderFailed` with kind `Transport` or `RateLimited` is a TRANSIENT end: the
adapter's own retries (seconds, before the stream starts) are already spent, or the stream broke
after it began. Dogfooding lost two long jobs to exactly this in one afternoon. Every other kind
(`InvalidRequest`, `Authentication`, `ContextWindowExceeded`, `Protocol`) ends the run as before.

Policy, headless only (an interactive user is present and decides):
- The host WAITS, then continues with one fixed user-role message (`PROVIDER_RETRY_MESSAGE`:
  the connection to the model failed, the last response was lost, nothing else changed,
  continue). The interrupted response stays in the journal as the core recorded it; the host
  adds nothing to history except that message.
- Waits are fixed schedules, not computed: `Transport` 5 s, 30 s, 120 s; `RateLimited` 60 s,
  300 s, 900 s. `--provider-retries N` (default 3, 0 disables) bounds CONSECUTIVE transient ends;
  a turn in which at least one provider response completed resets the count. Exhausted → exit 1
  as today, the last error printed.
- Cancel wins immediately during a wait (exit code as for any cancel).
- The wait is injected as a future (as the shell tool's `expiry`), so tests never sleep.
- Provider retries and continuations after a premature stop are separate budgets; a provider
  retry is not "progress" and not a continuation.
- Visible: the renderer prints `provider failed (<kind>): retry <n>/<max> in <s> s`. Countable:
  `run-report.py` reports `provider_retries` (the journalled `PROVIDER_RETRY_MESSAGE` inputs).

Must-pass (scripted provider, injected wait): Transport failure then success → the run completes,
one retry message in history, exit 0; three consecutive failures with N=3 → four attempts, exit 1;
a success between failures resets the count; `InvalidRequest` is never retried; cancel during the
wait exits as cancelled without a further request; `--provider-retries 0` behaves as before;
interactive mode is unchanged.

## 3c. Stall guard: summarizing without progress (dogfood run split4a)

A headless run that keeps replacing its context without ever changing the workspace is not
working, it is forgetting: run split4a made 855 requests and 40 replacements, read 698 files and
edited none. The host already owns "progress" (§3: the activity log records file changes and
successful `finish` calls), so it owns this guard too.

- The host counts CONSECUTIVE `ContextReplaced` events since the last progress (a workspace
  mutation recorded in the activity log, or a `finish` call of any status). Progress resets it.
- When the count reaches `--max-idle-summaries N` (default 6; 0 disables) the host cancels the
  turn and the run ends **stalled, exit 4**, printing
  `stalled: <N> context summaries without a change to the workspace — the task does not fit the
  configured context (see [context] in the environment), or it is too large for one job`.
- Headless only; an interactive user sees the summaries and decides. Read-only tasks are rare in
  unattended runs and have the flag.
- `run-report.py` reports `stalled_on_summaries: true|false`.
- §3b amendment: `Protocol` failures join the transient kinds with the `Transport` schedule — a
  malformed response (split4a ended on `unsupported chat tool type`) is the model's or the
  route's one-off, and the interrupted response is discarded as for any other failure.

Must-pass (scripted provider + scripted context policy that always replaces): N replacements with
no mutation → exit 4 and the message, no further provider request after the Nth; a mutation
between them resets the count (2N-1 replacements with one mutation in the middle → no stall); a
`finish` call resets it; `--max-idle-summaries 0` never stalls; interactive mode unchanged; the
count survives nothing — a resumed run starts at 0; Protocol failure then success → run completes
with one retry message.

## 4. Environments

`finish` joins the shipped environments' tool lists, and each family prompt gets a short
"Finishing" section in its own voice: end by calling `finish`; verify first; if blocked say
what you need; never end a turn with a question when running unattended.

## 5. Must-pass (observable behaviour, scripted providers, no phrase matching)

a0. (delegation) the parent ends its turn while its worker is still running → NO continuation
   message; the worker's notification wakes it; it then calls `finish(done)` → exit 0.
a. Model ends the turn with text only → exactly one continuation `UserInput` with the exact
   message; the next turn calls `finish(done)` validly → exit 0; journal shows both inputs.
b. Text-only turn ends, 4 times, with a (non-finish) tool call between each → 3 continuations,
   then `stalled`, exit 4. With NO tool call between them → 1 continuation, then exit 4.
c. `finish(blocked, needs)` → exit 3, stderr names the need, NO continuation.
d. `finish(done)` naming a command that never ran / ran with exit 1 / ran before a later
   `write` → the three exact Errors; the turn goes on; after re-running → `Finished.`, exit 0.
e. A later failing re-run of the named command invalidates the earlier success.
f. `["none"]`: accepted in a session without file changes; rejected after a `write`.
g. Interactive mode: a text-only turn end is NOT continued.
h. `--max-continuations 0`: a premature stop exits 4 at once. An environment without `finish`:
   behaviour exactly as before (exit 0 on `Completed`).
i. Cancellation during a continuation turn → 130, as any turn.

Live evidence (lead, `P1_LIVE=1`-style, reported not assumed): a task phrased so that models
typically stop to ask ("…let me know if I should proceed") runs to `finish(done)` without an
operator; and a task that cannot be done (needs a credential that is not there) ends `blocked`.

**Live result, 2026-09-20 (lead; both routes, sandboxed, `--yes`).** Task 1 ("…First tell me
your plan. Then let me know if I should proceed."): `claude-sonnet-5` and `gpt-5.6-sol` both
did the work and ended with an accepted `finish(done)`, exit 0, 0 continuations. On the Claude
route the first `finish` was REJECTED — it named `python3 -m unittest test_temperature -v`,
which it had not run in that form — the model then ran it and finished: the verification rule
worked on a real model. Task 2 (publish with a token that is not there, to an unreachable
host): both routes ended `finish(blocked)`, exit 3, naming what they need; neither invented
another destination. NOT shown live: the continuation path — with the Finishing prompt section
neither model stopped early, so continuation is proven by the scripted tests only. One run per
route and task.

