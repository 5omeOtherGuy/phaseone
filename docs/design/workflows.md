# Workflows — specification

## 1. Purpose and shape

A workflow is a script an agent writes at run time — Claude Code's shape — that starts
workers by ROLE under per-model caps (ADR-0053). It is an optional module like
delegation (ADR-0026): without these crates the harness is a plain coding agent, and
`p1-core` knows nothing of it. A script never names a model or reasoning level; it names
roles, and settings decide what each role is. One agent level (ADR-0050): a workflow
step is a worker, never granted the worker or workflow tools.

| Crate | Owns | Must not own |
|---|---|---|
| `p1-workflow` | The rhai script engine, the script API, roles/caps resolution, the step envelope, the run journal with prefix replay, cancellation, and the service seams (`WorkflowService`, `StepRunner`, `ModelResolver`, `WorkflowObserver`, `WorkflowSettings`). | No provider, no tool, no worker implementation: it depends only on `p1-contracts` — never `p1-core`, `p1-workers` or a tool crate. |
| `p1-tool-workflow` | The four model-facing tools (`workflow_start`, `workflow_status`, `workflow_result`, `workflow_cancel`) over the `WorkflowService` trait. | The engine's internals: it never constructs `InProcessWorkflows` and holds no run state. |
| the host | Composition: implements `StepRunner` over `p1-workers`' prepared start, `ModelResolver` over its environments and routes, parses `[workflows]`, chooses the run root, renders lines, calls `shutdown()`. | No workflow logic of its own — explicit composition in the native host, no registry. |

`api.rs` in `p1-workflow` is the frozen public surface: additions are allowed, renames and
removals are not.

## 2. Script contract

The script language is rhai (pinned `1.26.1`), JavaScript-like. Five functions plus
three bindings:

| Name | Meaning |
|---|---|
| `agent(prompt)` / `agent(prompt, opts)` | Runs ONE worker (a role's model and tool grant); returns the envelope map — never a naked value. |
| `parallel([thunks])` | Runs zero-argument closures concurrently; returns their results as an array in input order. |
| `pipeline(items, stage1, …)` | Runs every item through 1–6 stages; items proceed concurrently, stages run sequentially per item. |
| `phase(name)` | Labels progress (journalled, shown in status). |
| `log(message)` | Records a line in the run's log (`print` and `debug` go there too). |
| `args` | The run's argument map, a CONSTANT: assigning to it or `+=`-ing it is a script error. |
| `has(map, key)` | `true` when the map carries the key — the test for optional entries. |
| `json(value)` | Canonical JSON (keys sorted at every level, no whitespace) — how a value goes into a prompt. |

`agent()`'s `opts` — every key is known, a typo is a script error (`agent: unknown option
"rol"`), never a silently different run:

| Key | Type | Default | Meaning |
|---|---|---|---|
| `role` | string | `"worker"` | The role the step runs as. |
| `label` | string | none | Names the step in lines and the journal; part of the call id. |
| `phase` | string | the current phase | Recorded on the step request. |
| `schema` | map | none | The output contract the worker's `finish` gets (§5). |
| `tools` | non-empty array of strings | the role's grant | Replaces the role's grant for this one step. |
| `workspace` | string path | the run's workspace | Where the worker runs (§8). |
| `worktree` | slug string | none | The step runs in its own git worktree (ADR-0073): `<parent of the main worktree>/<main worktree name>-<slug>` on branch `task/<slug>`, made from the run's base commit when missing, reused untouched when there. The slug is lowercase ASCII letters, digits and `-`, starting and ending with a letter or digit, at most 64 characters; anything else, or `worktree` together with `workspace`, is a script error. A later step is pointed at the same tree with `workspace: r.worktree.path`. |

**Idioms a writer needs.** Maps are `#{ key: value }`. Work given to `parallel` must be
deferred as a closure — `|| agent(..)` — because `parallel` takes functions: passing
`agent(..)`'s result is the error `parallel: every element must be a function (\`||
agent(..)\` or \`|x| ..\`), not map`. A closure held in a variable is called as
`f.call(args)`; closures capture by value, so in `dims.map(|d| || agent(..))` each thunk
holds its own copy of `d`. There is no `null` (`()` is the unit): test optional entries
with `has(map, key)`. Strings concatenate with `+`; `+=` on an array appends its items.

**The sandbox.** The engine is built raw (`Engine::new_raw()`) with only the reviewed
packages (LanguageCore, Arithmetic, Logic, BasicString, MoreString, BasicArray, BasicMap,
BasicIterator). No module resolver is set, so `import` finds nothing; `eval` is a disabled
keyword (a parse error); `sleep` is overridden to fail with `sleep is not available to
scripts`. Nothing else exists — no time, randomness, files, network, environment or
process access (`timestamp`, `now`, `rand`, `read_file`, `http_get`, `connect`, `env`,
`exec` all fail) — every one a runtime error with `[line L, column C]` and never a step
started.

**Engine limits.** `max_operations` 5,000,000; `max_call_levels` 24; expression depths
32/32; `max_variables` 256; `max_functions` 256 (closures count, which also bounds the
thunks of one script); `max_modules` 1.

The three DATA limits are sized to the RUN, not to one value. rhai checks `max_string_size`,
`max_array_size` and `max_map_size` against the SUM of every string, array item and map
entry inside the whole value a call returns (`eval/data_check.rs`
`calc_array_sizes`/`calc_map_sizes`), and the result of a native function is checked like
any other — rhai has no way to exempt a value. The envelope `agent()` returns is the
HOST's data, one per call, so one `parallel()` of a dozen done envelopes, a map a script
builds from several envelopes, and one verbose envelope are all ONE budget (issue #121).
The engine therefore sizes each limit to the run's own step cap — `min(max_steps, 64)`
envelopes' worth, one envelope being 64 KiB of strings, 4096 array items and 4096 map
entries — with the cap a constant (`engine.rs` `DATA_BUDGET_ENVELOPES`). At the default
`max_steps = 200` the limits ARE the cap: 4 MiB of strings, 262,144 array items, 262,144
map entries in one script value, which a run of any step cap cannot exceed. rhai gives
each VALUE its own three sums, so the sandbox's ceiling is that cap times the concurrency:
64 thunks × 4 MiB of strings = 256 MiB, and near 1.5 GiB for a script that fills a string,
an array and a map in every thunk at once — the cap is what keeps the ceiling independent
of the operator's own step cap.

A script's OWN strings, arrays and maps stay bounded by the same numbers (a `max_steps = 2`
run can build a 128 KiB string, not more), and building them is still charged against
`max_operations`. Because the budget is per VALUE and capped, a fan-out whose envelopes sum
past 4 MiB of strings — more than 64 full-size (64 KiB) envelopes, about 136 at the 30 KB
the issue's steps returned — must be SPLIT into several `parallel()` calls; `max_steps`
200 still allows several such batches.

**The bounded thread rule.** rhai has no async VM: each run's script executes on its own
OS thread (`p1-wf-script`), and `agent()` blocks that thread on the caller's tokio handle
(the crate owns no runtime). Concurrency is one OS thread per in-flight thunk
(`p1-wf-thunk`) from a pool of `max_threads` slots (default 64, clamped to at most 64 so the §2 memory ceiling holds). A thunk that finds no
slot free runs INLINE on its caller's thread — nothing ever waits for a slot, so nested
`parallel` inside a `pipeline` stage cannot deadlock at any bound, including 1. Results
keep input order; every thread is joined before the first error (in input order) is
reported.

**Pipeline semantics.** Each stage takes exactly ONE argument — the previous stage's
result, the item itself for stage 1 — and may return any value. There is no barrier
between stages: one item may still be in stage 1 while another is in stage 2. Items run
concurrently under the same thread bound; the result array is in input order.

**The example every prompt carries** (identical in the five shipped environments;
`tests/prompt_example.rs` runs it on the engine):

```rhai
let task = if has(args, "task") { args.task } else { "the user's task" };
let dims = ["correctness", "tests", "docs"];
let schema = #{ type: "object", properties: #{ findings: #{
    type: "array", items: #{ type: "object",
    properties: #{ title: #{ type: "string" }, file: #{ type: "string" } },
    required: ["title", "file"], additionalProperties: false }
} }, required: ["findings"], additionalProperties: false };
phase("review");
let reviews = parallel(dims.map(|d| || agent(
    "Review " + d + " for: " + task,
    #{ role: "reviewer", label: "review:" + d, schema: schema }
)));
let findings = [];
for r in reviews {
    if r.status == "done" { findings += r.value.findings; }
}
phase("verify");
let verdicts = pipeline(findings, |f| agent(
    "Refute or confirm: " + json(f),
    #{ role: "verifier", label: "verify:" + f.title }
));
#{ confirmed: verdicts.filter(|v| v.status == "done"), reviews: reviews.len() }
```

## 3. Roles and caps

`[workflows]` in `settings.toml` (`deny_unknown_fields` — an unknown key is an error
naming it) has four keys: `roles` (a map of name → `RoleSpec`), `caps` (a map of WIRE
model → attempts per run; absent = unlimited), `max_steps` (`agent()` calls one run may
make, replayed calls included; default 200) and `max_threads` (thunk threads at once;
default 64). A `RoleSpec` is `model` (`environment/profile[:effort]`, ADR-0049),
`fallback` (an ordered list of the same references, the chain §3 "Fallback" walks,
ADR-0054) and `tools` (tool MODULE names granted in addition to `finish`, which every
worker always gets; the call's own `tools` replaces them for one step). The shipped
defaults, as TOML:

```toml
[workflows]
max_steps = 200
max_threads = 64

[workflows.caps]
"claude-fable-5" = 3

[workflows.roles.worker]
model = "deepseek2/deepseek-v4.1-flash"
fallback = ["gpt/gpt-6-sol", "claude/claude-opus-5-5"]
tools = ["read", "grep", "edit", "shell"]

[workflows.roles.reviewer]
model = "claude/claude-opus-5-5:high"
tools = ["read", "grep", "shell"]

[workflows.roles.verifier]
model = "deepseek2/deepseek-v4.1-flash"
fallback = ["gpt/gpt-6-sol"]
tools = ["read", "grep", "shell"]

[workflows.roles.judge]
model = "claude/claude-fable-5"
tools = ["read", "grep"]
```

A user table is laid OVER the shipped defaults (`WorkflowSettings::overridden_by`): a
role or a cap the user names replaces the shipped one of that name — WHOLESALE, so a role
whose table names a `model` and no `fallback` has an EMPTY chain — the others stay;
`max_steps` and `max_threads` come from the user table (200/64 when it omits them — the
same values). `StartRequest::role_models` maps a role name to another model reference for
one run — the role KEEPS its tool grant and its chain, the override replaces the head
only; `workflow_start` does not expose it, it is the caller's (host) field, and a name not
in the table is a preflight error (`role_models names unknown role …`).

At preflight, BEFORE any worker starts, EVERY reference of EVERY role's chain — the head
and each fallback, used by the script or not — is resolved through `ModelResolver`: a
broken table fails the start. The resolver returns the `ResolvedModel` (`reference`,
`environment`, `profile`, `effort`, `wire_model`); caps count the WIRE model, so two roles
or two profiles of one model share one counter and no renaming in settings can multiply a
scarce model's budget.

**What a cap counts.** Attempts = starts + repairs, per run, per wire model, checked and
spent under one lock (two concurrent thunks can never both take the last attempt). The
(cap+1)th attempt writes a `Capped` journal line and is refused: an envelope's `error` is
`quota_exceeded: <wire_model> used=<u> limit=<l>` and `attempts: 0` for a refused start —
no worker was built; a refused repair keeps the invalid `value` and the attempts already
spent, its error ends with ` (repair)`. A cap is NOT a route failure: a link a cap refuses
is skipped — the next link of a chain may run (§3 "Fallback"), and a role with no fallback
fails as it always did — and that skip costs one `Capped` line and one count, never an
attempt. `max_steps` is checked even earlier: the (max_steps+1)th `agent()` call returns
`failed` with `error: max_steps: <n> reached`, no dispatch, but a `Result` journal line.

### Fallback (ADR-0054)

A role's `fallback` list is walked by ONE step when the model before it failed on its
ROUTE — the host's `StepRunner` reports `StepEnd::RouteFailed` when the worker could not
run at all or its turn ended on a provider failure (an exhausted account, ADR-0046's
kind; an unreachable route; a route refusing the model — a `TurnEnd::ProviderFailed`
after the host's own retries). Nothing else moves the step on: a `failed` step that ran (a
wrong answer), a `blocked` one, a cap (`quota_exceeded` stays final for that link), a
schema failure and a cancellation do not; the one schema repair stays in the worker that
produced the invalid result, so a repair turn that loses its route ends the step instead
of hopping.

Every model the chain turns to is a `Dispatch` of its own — one worker, charged to that
model's cap — and each hop is a `Fallback` journal line (`{call, from, to, error}`)
written BEFORE the next model is dispatched. A capped link is skipped as `capped`; if no
link ends the step, the step ends `failed`: `route: <last error>` when a link's route
failed, else the last cap refusal (`quota_exceeded: …`). The step line and the envelope
name the chain walked — `worker → deepseek2/deepseek-v4.1-flash route failed →
gpt/gpt-6-sol; w7` — the envelope's `models` carries every link with `moved_on`
(`route_failed` / `capped`, absent on the one the step ended on), and the run counts
`fell back`. Each link is one more attempt. A replayed step keeps the chain it recorded; a
re-run step starts from the head again.

## 4. The step envelope

`agent()` returns a rhai map with exactly these keys:

| Field | |
|---|---|
| `step` | The call id: 16 hex chars (§6). |
| `label` | The call's `label`, or `()`. |
| `status` | `"done"`, `"blocked"`, `"failed"` or `"cancelled"`. |
| `value` | The value rule, below. |
| `schema` | `"not_requested"`, `"passed"`, or `{"failed": ["…", …]}`. |
| `evidence` | ADR-0051's label for a `done` step (`commands passed: …` / `not verified; parent verification required`). |
| `attempts` | Starts + repairs spent; 0 for a step refused before dispatch. |
| `worker` | `<id> (<route/model>)` of the worker that ran it, or `()`. |
| `needs` | A blocked step's need, or `()`. |
| `error` | A typed failure, or `()`. |
| `models` | The chain the step walked (§3 "Fallback"), head first: `[{model, moved_on}]`, `moved_on` `route_failed`/`capped` where the step moved on. `[]` only when the step was refused before it reached a model (an unknown role, `max_steps`). |
| `worktree` | Only on a step that asked for a worktree and got it (ADR-0073): `{path, branch, head}` — `head` is the tree's `HEAD` after the step ended. Absent otherwise (a replayed step returns its recorded envelope unchanged). |

The `schema` states mirror the `finish` tool's own check (§5).

**The value rule.** For a `done` with a contract that passed: the accepted `result`
(a JSON value). For a `done` without a contract: the `finish` summary text. For a result
that failed the check: the last rejected `result` is kept — before the repair and after
it. For `ended without finish`: the worker's last message. For blocked, failed, cancelled
and every step refused before dispatch: `()`.

**Every `error` prefix.**

| `error` | Meaning |
|---|---|
| `quota_exceeded: <wire_model> used=<u> limit=<l>` (with ` (repair)` for a refused repair) | The run's attempt cap on that wire model (§3). |
| `invalid_output: <e1>; <e2>` | The result failed the contract twice, after the one repair (§5). |
| `max_steps: <n> reached` | The run's `agent()` budget is spent. |
| `unknown_role: <name>` | The role is not in the effective table. |
| `route: <error>` | No model of the role's chain could run the step — the last link's route failure. |
| `ended without finish` | The worker's turn completed with no accepted `finish` call, after one repair turn (§5, ADR-0072). |
| `worktree: <slug>: <reason>` | The step's worktree could not be made or reused, before dispatch (ADR-0073): the run has no base commit (its workspace is not a git repository), the path exists and is not that branch's worktree, or git's own error. |
| `worktree_busy: <slug>` | A running step of this host holds that worktree; refused at once, before dispatch (ADR-0073). |
| anything else | The host's `StepRunner` reason (its `Err` string, e.g. an unknown environment), or `journal: …` when the `Dispatch` line could not be written. |

## 5. Structured output

A step with a `schema` gets it as the worker's `finish` `OutputContract`
(`FinishTool::with_output_contract`, ADR-0053 item 5). The worker passes its structured
value as `finish`'s `result` field together with `"done"`; the contract is the JSON
Schema subset `type` (one of object, array, string, integer, number, boolean, null),
`enum`, `required`, `properties`, `additionalProperties: false` only, `items`,
`minItems`, `minimum`, plus the ignored documentation keywords `description` and
`title` — nothing else, and no nesting deeper than 32. The schema is validated once at
construction, so a malformed contract fails the build BEFORE any worker is started. The
tool's rules, texts and error wording are completion.md §2 "Structured result" (at most
32 errors, each at most 300 characters).

**The repair flow, exactly one round.** A `done` whose result is `Failed(errors)`:
1. the cap is checked AGAIN (a capped repair is the §3 refused-repair envelope);
2. ONE repair turn runs in the SAME worker — never a new worker, never a new request —
   with the message
   `Your result did not match the required schema:` + one `- <error>` line each +
   `Call finish again with a corrected "result" that matches the schema of the "result" parameter.`;
3. a `done` that passes → `status: "done"`, `schema: "passed"`, `attempts: 2`;
4. a second `Failed(errors)` → `status: "failed"`,
   `error: invalid_output: <errors joined by "; ">`, the last rejected value kept,
   `attempts: 2`; any other end (blocked, ended without finish) is that end, `attempts: 2`.

**A turn that ends without `finish` (ADR-0072)** gets the same one round: when a step's
FIRST turn ends without an accepted `finish` call,
1. the cap is checked AGAIN (a capped nudge is the §3 refused-repair envelope, the
   worker's message kept as the value);
2. ONE repair turn runs in the SAME worker with the message
   `You ended your turn without calling finish. Call finish now: status "done" with your result (and the evidence), or "blocked" with what you need.`;
3. its end is the step's end, `attempts: 2`; a second end without `finish` stays
   `ended without finish` with the worker's last message as the value.

A step gets at most ONE repair turn: a schema repair turn that ends without `finish` is
that end and is not nudged again.

## 6. Journal and replay

Each record is one JSON line in the run's `journal.jsonl`, append-only, written with one
`write_all` on an unbuffered file and flushed per record — a crash loses at most the line
being written and never reorders. Reading back, a final line without its newline is
ignored (a crash-interrupted write); any other unreadable line is an error, because
silently skipping a `Dispatch` would under-charge the caps.

| Kind | Fields | When |
|---|---|---|
| `started` | run, script_hash, args, resumed_from, base (only when the run has one, ADR-0073) | The first line. |
| `phase` | name | Every `phase()`. |
| `dispatch` | call, label, role, model, wire_model, attempt, prompt, opts | **Before the worker starts** — one per model a chain turns to. |
| `capped` | call, wire_model, used, limit | A dispatch refused by a cap, before anything ran. |
| `fallback` | call, from, to, error | One hop of a role's chain, BEFORE the next model is dispatched. |
| `replayed` | call, from | A call answered from the old journal without a worker. |
| `result` | call, envelope | After each step. |
| `ended` | outcome, counts, error | The last line. |

**Caps rebuilt conservatively.** On resume, EVERY `Dispatch` line of the old journal —
matched or not — is charged to the new run's counter before the script starts; nothing
is refunded, and since the line is written before the runner is called, an attempt that
crashed mid-step still counts.

**The call id** is FNV-1a 64 over `label \0 prompt \0 canonical opts` (canonical JSON:
object keys sorted at every level, no whitespace), 16 hex chars. Never a sequence number,
because `parallel` reaches `agent()` in a different order each run; two calls with the
same label, prompt and opts have the same id.

**The replay rule.** `resume_from: wfK` loads `run_root/wfK/journal.jsonl`. Replayable
entries are its `Result` records with status `done` only — a failed, blocked or cancelled
step is never replayed, it must run again. Matching is by call id among the entries not
yet taken, not by position; two calls with equal ids take the entries in journal order.
The first call with no match LATCHES replay off for the rest of the run — the prefix
rule: everything after a changed call re-runs even if it looks unchanged, because its
inputs may have come from the changed call. A replayed call writes `Replayed { call,
from }` plus its own `Result` line, costs no attempt, and its step line carries
`replayed: true`.

**Run directory layout.** Each run owns `run_root/<run id>/` with `script.rhai` (the
submitted source), `args.json`, `journal.jsonl` (above) and, at the end, `result.json`
(the `RunReport`). Run ids are `wf<N>`; numbering continues after the highest `wf<N>`
already in the run root, and the directory is taken with `create_dir` (not
`create_dir_all`), so a number another process took fails and the next is tried — ids
stay unique across processes and `resume_from: wf3` always names `run_root/wf3`. A
parse error or a preflight refusal creates no run directory at all.

## 7. Host

The four tools (`p1-tool-workflow`, effect `Delegates`; `ToolFace` renames one for a host
that needs it). All take a JSON object input; unknown fields and freeform text are
`Invalid input for <tool>: …`.

| Tool | Input | Ok content |
|---|---|---|
| `workflow_start` | `{"script", "args"?, "resume_from"?}` | `Started workflow wf1. You will be notified when it ends; do not poll.` — with resume: `Started workflow wf2, resuming wf1. …` |
| `workflow_status` | `{"id"}` | Running: `Workflow wf1: running — phase review, 3 steps started, 2 ended, 1 replayed` then the last log lines, two-space indented (phase `none` before any `phase()`). Ended: the report's first line, below. |
| `workflow_result` | `{"id", "wait"?}` | The report: first line, `error: …` when present, `steps:` (below; at most 200 lines, then `… <n> more in result.json`), `result:` the pretty value (at most 16 KiB, then truncated), `run dir: <path>`. `wait: true` blocks through `WorkflowService::wait`, `Cancelled` if the tool's own token fires first. |
| `workflow_cancel` | `{"id"}` | `Workflow wf1 cancelled.` |

Errors: `No workflow <id>.`; start adds `Script does not parse: <message> [line L,
column C]`, `Cannot start workflow: <reason>` (preflight, io) and `Cannot start
workflow: the workflow service has shut down.`.

**Where they are mounted.** Through `p1_tool_workflow::all(Arc<dyn WorkflowService>)` —
an assembly that does not compose them has none, and an unassembled tool cannot be
dispatched. The prompt section is conditional
(`{{#tool:workflow_start}} … {{/tool:workflow_start}}`, nested sections for
`workflow_result` and `workflow_cancel`), identical in the five shipped environments, so
an agent without the tool never sees it. A step can never get them: the engine refuses a
grant naming an empty name, `finish`, `delegate`, any `worker*` or any `workflow*`
module, in every role table at preflight and in every call's `tools` (one level).

**The prompt rule**: the section is titled "# Workflows (only when the user asks)" — a
workflow only when the user explicitly asks for one, plain work and workers otherwise;
never split a workflow into several to get around a cap; inspect the counts, not just
the script's returned value.

**The lines** (`workflow_result`'s rendering; the run line is also `workflow_status`'s
ended answer):

```
Workflow wf1: completed with issues — 2 steps (1 replayed): 1 done, 0 blocked, 1 failed, 0 cancelled; 1 not verified; 1 capped; 0 invalid output; 1 fell back
  review reviewer → route/model [w3 (route/model)] done — schema passed; not verified; parent verification required; replayed; attempts 2
  c2 judge → other/model failed — schema not requested; quota_exceeded: cap 3
  c3 worker → deepseek2/deepseek-v4.1-flash route failed → gpt/gpt-6-sol [w7 (gpt/gpt-6-sol)] done — not verified; parent verification required
```

A step line is `  ` + label (or call id) + ` ` + role + ` → ` + the model chain + ` [<worker>]` +
` ` + status + ` — schema <schema>` +, only when present and in this order, `; <evidence>`,
`; replayed`, `; attempts N`, `; <error>`. The chain is the role's model, then every link
the step moved on from as `<model> route failed` or `<model> capped` before ` → `
(ADR-0054). "Verified" is never printed; the evidence is copied as is. The counts make a
script unable to hide failed, blocked, not-verified, capped or fell-back workers. Outcome
selection considers only failed, blocked, cancelled and capped: `Completed` when the script
returned and all four are zero, `CompletedWithIssues` when the script returned but any is
non-zero. `not_verified` and `fell_back` remain visible but do not by themselves change the
outcome. `Completed` is not independent acceptance; every count, including not-verified and
fell-back counts, and the step evidence must still be inspected. `Failed` when the script
itself did not return (parse is refused before a run exists; runtime error, limit, panic,
non-JSON return), `Cancelled`.

**The ONE notification.** The engine reports through `WorkflowObserver` (every method a
no-op by default): `run_started`, `phase`, `log`, `step_started`, `step_ended`,
`thunk_failed`, `run_ended`. `step_started` fires only when the step's worker is known — the runner
returns the `WorkerRef` at the end — so it is not a start signal. `thunk_failed` fires
when one `parallel`/`pipeline` job ends with an error, before its siblings are joined
(tests synchronise on it; the host ignores it). `run_ended` fires
after `result.json` is written, the `Ended` line journalled and the report stored, and
is the ONE place the host wakes the parent from: one notification at the run's end,
never one per step.

**Background lifetime and shutdown.** `start` compiles, preflights and starts NOW, in
the background; nothing is started on `Err`. Runs are retained for the service's
lifetime — a report stays retrievable by id however late anyone asks. `status` never
blocks; `wait` resolves as soon as the run has ended, cancel-safe and repeatable;
`cancel` cancels every in-flight step (a spinning script dies within a few operations, a
blocked `agent()` is dropped), stops the script, journals its `Ended`, is idempotent,
and returns only once `Ended` is journalled. `InProcessWorkflows::shutdown()` cancels
every running run (each journals `Ended { outcome: cancelled }`), joins the script
threads, and afterwards every fallible call returns `ShutDown`.

**The host, as landed (job 5).** `p1-host` feature `workflows` (default, over `delegation`).
`HostModelResolver` resolves `environment/profile[:effort]` through `p1 models` and takes the wire
model from the environment's route; `HostStepRunner` waits for capacity, starts each step through
`start_prepared` with the role's or the call's grant (never a worker or workflow tool), gives the
worker's `finish` the contract, reads the structured result from the worker's own outcome cell, and
reports the worker's last turn end (`TurnEnd::ProviderFailed` → `StepEnd::RouteFailed`, ADR-0054) so
a chain can hop; a repair is `continue_child` on the same worker. One `ChildBuilder::build_child`
serves direct workers and steps. `HostWorkflowObserver` prints one stderr line per step
(`· workflow wf1 review:bugs (reviewer → claude/claude-opus-5-5:high; w3) done — schema passed; not
verified; parent verification required`), one per phase/log, one run line, and sends the parent ONE
inbox notification (`Workflow wf1 ended (completed). Use workflow_result to read its result.`);
step workers never notify the parent. `wait_for_work` keeps a headless parent alive while a run is
in flight; runs shut down before workers. `p1 workflow run FILE [--arg K=V]… [--args FILE]
[--role R=E/P[:effort]]… [--resume-from ID] [--out DIR] [--workspace DIR] [--session FILE]
[--max-workers N] [--yes]` composes the same services with no parent agent and exits 0 completed,
2 completed with issues, 1 failed, 130 cancelled. Run root: `<session>.workflows/` next to a session
file, else `$XDG_STATE_HOME/p1/workflows` (or `~/.local/state/p1/workflows`), `--out` overrides.
`scripts/run-report.py` reports `workflows: [{id, outcome, counts, steps, run_dir}]` next to a session.

**The run root rule**: the service is built with one `run_root`; every run gets
`run_root/<run id>/` (§6). Choosing it is the host's composition; the module fixes only
the layout and the numbering.

## 8. Stated limits / deferred

Deferred explicitly (ADR-0053 item 8; each returns only when a real run shows the need):
required checks run by `finish`; workspace leases and artifact identity; budget groups;
a durable quota ledger; fuel accounting beyond the engine's limits; detached execution;
sub-workflows. Verification is a script pattern (checker steps, refuter votes), not an
engine feature — the run envelope's counts make unverified and failed steps visible
whatever the script returns.

- **Workspace rule**: a step runs in the run's own workspace (the `workspace` the run's
  caller gave `StartRequest`) or in a path the script itself names with the call's
  `workspace` opt; the opt wins for that step. The engine passes it through and confines
  nothing; the host's workspace rules (ADR-0025) apply as for any worker.
- **Worktree rule** (ADR-0073): a step with `worktree: "<slug>"` runs in its own git
  worktree. The host resolves the run's base commit (`git rev-parse HEAD` of the run's
  workspace) when it starts the run; a resumed run keeps its predecessor's. The host holds
  a step's worktree for the whole step — its fallback links and its repair turn — and
  refuses a second step on it with `worktree_busy:`. Nothing is ever deleted, reset,
  cleaned or forced: a finished tree is removed with `git worktree remove`, by the rule
  for every worktree.
- **Thread cost**: one OS thread per in-flight thunk, bounded by `max_threads`
  (default 64, inline fallback), plus one thread per running script.
- **Runs do not survive the process, journals do.** A run lives in its service; its
  `journal.jsonl` and `result.json` stay on disk, and `resume_from` builds the next run
  on them (§6).

## 9. Tests

`crates/p1-workflow/tests/` drives the real engine over a scripted `StepRunner`
(`support/`): `runs.rs` proves the run end to end — parse errors, run-id numbering,
`result.json`, nested `parallel` bounded at `max_threads` 1 and 4, caps shared across
roles, the one schema repair and the capped repair, dispatch journalled before the
runner sees it, cancellation of a spinning loop and a blocked step, shutdown,
status/wait, `max_steps`, unknown role and option, role-model overrides, preflight
refusals, every envelope shape, constant `args`, located script errors. `replay.rs`
proves the replay rules: an unchanged run replays everything, an edited middle call
re-runs it and everything after, caps are rebuilt from every old dispatch, failed steps
are not replayed, parallel calls match by content. `sandbox.rs` proves every escape
vector fails and every engine limit holds; `size_limits.rs` proves a completed `parallel`
of twelve large envelopes comes back whole, that one verbose envelope does too, that a
script's own string is refused past its run's budget, and that a 200-step run is still
capped at 64 envelopes' worth; `prompt_example.rs` runs the prompts'
example on the shipped settings; `fallback.rs` proves the chains (ADR-0054): a route
failure on the head hands the step to the next link with
`dispatch/fallback/dispatch` journalled and `fell_back` counted, a capped link is skipped
and counted (a chain can never pass a capped model past its cap), a whole chain failing
ends `failed — route: …`, a step that ran and failed does not fall back, a schema repair
stays in its worker, and resume replays the recorded chain while a re-run starts at the
head. `crates/p1-tool-workflow/tests/tools.rs` proves
the four declarations and faces, every Ok and error text, the wait semantics and the two
rendering bounds; `crates/p1-tool-finish/tests/result.rs` proves the `OutputContract`
subset and its path-worded errors; `p1-workers`' in-crate tests prove the prepared
start; `api.rs`'s unit tests pin the shipped defaults (DeepSeek worker with its chain),
the override rule and the journal round-trip. The host's own suites prove the seams:
`crates/p1-host/tests/workflow_fallback.rs` runs `p1 workflow run` over scripted providers
and shows a provider failure becoming a hop (and a `finish`-less turn staying a plain
failure), `workflow_settings.rs` the table over the shipped defaults.
