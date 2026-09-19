# p1 — modules and seams

Draft v1, Fable + Astra; technical proposal, not owner-approved. Product direction:
`pillars.md` rev 2. The first working harness must already have replaceable
providers and tools. Delegation is available, never required; modes come later.

## 1. What a module is

A module is an independently selectable implementation or capability behind an
explicit Rust interface. A public module boundary normally maps to a crate;
private helper modules need not. A module can implement several closely related
interfaces. Every tool remains an independently selectable module.

Compose using normal Rust constructors and explicit dependencies. No dynamic
plugin loader, service locator, hidden global registry, or new dependency-injection
framework. The application composition root supplies concrete implementations;
an embeddable library caller can be that root too.

Dependency rules:
- Core depends on small contracts, never concrete tools/providers or storage/UI.
- Providers and tools depend on their contracts and explicit supporting libraries.
  Sibling implementation internals remain private. Shared filesystem/process
  helpers are allowed; duplicating them to obey a blanket dependency ban is not.
- A worker service can depend on the core and an injected agent factory. The
  delegation tool depends on the worker interface, not its runtime implementation.
- Keep contracts cohesive; do not recreate Nexus as a huge shared-types crate.
  Worker/session contracts may live outside the minimal loop contracts.

Prove a seam through real consumers and replaceable implementations. Claude/GPT
exercise the provider seam, and memory/JSONL exercise storage. Do not build a
second shell, UI, or worker backend solely to satisfy a two-implementations rule.

## 2. Pieces and ownership

Names below describe boundaries, not a frozen crate list.

| Piece | Owns | Does not own |
|---|---|---|
| Core contracts | Request/history items, tool calls/results, provider stream, small control/event types | Concrete providers/tools, project settings, terminal types |
| Agent core | One agent's request → stream → tool calls → repeat loop; state, cancellation, steering, ordered boundaries | Model-specific choices, prompt prose, filesystem implementation, worker scheduler |
| Providers | API/transport/auth translation, streaming, native replay metadata, capability validation, cache/reasoning request mapping | Tool execution, choosing the agent's task prompt or toolset |
| Each tool | Its declaration, argument validation, execution, output semantics; optional model-facing variants | Provider HTTP/wire formats, terminal widgets, sibling tool internals |
| Environment assembly | Resolve agent specification to actual provider, prompt, exact tools, and policy implementations | Executing turns or implementing those components |
| Session store | Append/read committed session records in memory or a file | Choosing context, model routes, or interpreting business goals |
| Context policy | Construct the next model-visible history from session state; model-appropriate context controls | Storage format, permissions, tool execution |
| Worker service (optional) | Child lifecycle, handles, completion, cancellation, continuing a child | Mandatory lead role or task decomposition policy |
| Frontend/host | User input, event rendering, application lifetime and concrete wiring | Provider/tool internals |

Shared workspace/process helpers can support multiple tools. This is different
from making a mandatory `tool-files` bundle that prevents replacing read, edit,
and patch independently.

## 3. Provider seam

Request in, stream out, plus capability/option validation. Capabilities describe
an actual model + endpoint route; family labels alone are not sufficient.

- Requests carry the effective system prompt, prepared history, exact tool
  declarations, model options, and cancellation. Transports stay inside providers.
- Stream deltas preserve content-block identity/order and tool-call IDs. A completed
  assistant item retains text, calls, and native continuation metadata needed for
  faithful follow-up. Do not flatten everything into one string.
- Use one terminal outcome: completed, failed, or cancelled. Setup may fail before
  a stream exists. Unexpected EOF is a failure, never implicit completion. Ignore
  or reject events after termination. Partial calls are never executed.
- Preserve raw tool arguments until a complete call exists; JSON-function tools
  validate JSON, freeform tools validate their own format. Invalid input becomes
  an explicit tool error, not an application crash or a guessed repair.
- Usage/cost are optional, particularly on interruption. Preserve final usage when
  available; never invent it to satisfy a universal "usage before finish" rule.
- Opaque replay data is versioned and tagged with its origin route/model. The
  adapter states compatibility; the host decides whether a proposed switch can
  proceed. Cross-provider fallback must translate supported content or reject the
  transition explicitly, never silently strip required reasoning/signatures.
- Unsupported requested behavior is diagnosed. Provider defaults and explicit
  options are distinguishable. Retries must not duplicate already executed tools;
  partial stream recovery is explicit rather than replaying visible effects.

Tool declaration support is driven by the two real adapters: JSON-schema function
calls and the native/freeform shapes required by chosen editing tools. Preserve a
namespaced extension path for native options; do not prebuild every provider tool
kind. Provider-hosted execution (such as hosted search) is a separate capability:
the local tool executor must not execute it again. Its support can wait.

## 4. Tool seam and model adaptation

A tool module offers a descriptor and executable implementation. Assembly chooses
what to instantiate. The descriptor and execution target are registered together
under a unique call name, so the prompt cannot advertise a different interface
from the one executed. Record a stable implementation/variant identity separately
from the model-facing name; changing an implementation must not silently inherit
the previous implementation's grants or resolve old calls as a different tool.

Tool input is validated at its boundary. The tool receives explicit context
(workspace access, cancellation, any declared services), never Iris's global
ToolState or an untyped service bag. A file tool's confinement/atomic-write and
read-before-mutate invariants belong in the tool/backend even when interactive
approval is disabled. General execution authorization is a separate policy.

Separate meaning from presentation: result status + structured result/artifacts;
model-visible content produced by the selected tool variant; optional neutral
presentation metadata. No terminal/UI types in tools. Do not require every tool
to create three copies of a large result: the straightforward case is one text
result; share/reference larger data when necessary. Persist the exact content
sent back to the model, not just a result that would render differently later.

Variant rule: keep a shared implementation when semantics are shared; use a
wrapper for different descriptions/schema/rendering; use another tool module
when behavior actually differs. Read, edit, patch, shell, and search are separate
selectable tools; edit and patch may share a small mutation library. No forced
per-model duplication and no forced common interface that erases native strengths.

## 5. Environment assembly

Resolve `(model route, task/role, explicit configuration)` into an agent instance.
A family preset is a default recipe, refined by model/route capabilities and task
needs. Tools OFFER implementations/variants; configuration CHOOSES them; providers
VALIDATE and TRANSLATE them. The core does none of this selection.

A small explicit factory catalog at the composition root is enough to map config
names to compiled constructors. It is not a global inventory shown to every agent.
The agent owns only its assembled tools. Missing required modules, duplicate tool
names, incompatible options, and unsupported declarations fail before a run starts.
A generic preset, if included, is selected explicitly rather than silently replacing
an incompatible specialized one. Config cannot load an implementation not compiled in.

Start with whole prompt files and small explicit substitutions for actual tool
names/context. Tool schemas/descriptions come from the same resolved descriptors.
No general prompt-fragment engine. Prompt/tool coherence gets tests for both
Claude and GPT environments. Credential material is never stored in the resolved
public environment manifest; log model/route/options and module identities safely.

Single agents and delegated agents use the same factory. No mandatory lead profile.
A worker receives the necessary task/context, not the whole parent's transcript.
An already running agent keeps its environment until an explicit transition at a
safe boundary; model switching must validate history/replay compatibility.

## 6. Policy interfaces, not a general hook platform

Start with named interfaces at actual decision points:
- Context preparation before each provider request, defaulting to current history.
- Execution authorization before side effects, default supplied by the host.
- An optional turn-completion policy only if the first workflow needs continuation
  beyond normal tool-loop completion; otherwise the core ends the turn normally.

Permit/deny/ask is an authorization outcome, not a tool property that embeds UI.
An ask is delivered through host I/O with cancellation; a headless host must have
an explicit answer/deny policy rather than wait for nonexistent user input.

Observation uses events. Add an after-tool interception interface only when a
concrete consumer must transform behavior; metrics do not justify one. A context
module may ask a provider to summarize through its ordinary request interface;
no compaction-specific methods on every provider or giant governor on the core.

## 7. Session semantics — proposed choice

Use a small append-only canonical session journal for resumable conversation
state, with memory and JSONL backends. In-memory state is its incremental projection,
not an independently persisted second truth. Do not event-source all UI, metrics,
stream deltas, scheduler internals, or every piece of transient state.

The core owns ordered state transitions and accepts a narrow asynchronous commit
sink; a host supplies its store adapter (memory is the default). No filesystem or
session-directory logic in the core. Committed history uses stable item/call IDs.
Record the effective prompt/environment and model-visible context changes so that
resume preserves what was actually sent, including opaque replay data. Exact raw
HTTP capture is not needed for this contract.

Durable hosts acknowledge commits at meaningful boundaries: user input before
request; a completed assistant/tool-call item before execution; tool-start intent
before side effects; result before the next request. Streaming text can be shown
before commit; an interrupted stream is recorded as interrupted, not as a complete
assistant response. A failed commit stops forward progress with a clear error.

A crash after a side effect but before its result is recorded is ambiguous. On
resume, mark that invocation interrupted/unknown and reconcile its state before
retrying. A JSONL log does not grant exactly-once execution. File sync policy and
truncated-tail recovery must be explicit if restart durability is claimed. Full
crash recovery is not automatically required in the first usable demo.

Context preparation can use in-memory projections; it need not replay the whole
log per request. Context replacements are recorded when they change future model
input; compaction algorithms remain outside the core.

## 8. Optional delegation seam

The delegation tool consumes a typed worker handle API: start, inspect/wait,
receive completion, cancel, and continue a retained child session. The concrete
in-process service owns child cores assembled by an injected factory. It can be
absent entirely; the plain coding agent still works.

A task starts when accepted, not when someone polls. Results remain retrievable
by ID. Notifications wake the parent driver at a safe boundary, including while
it waits, but are not the only record of completion. Retained status/results let
a consumer recover from a missed event. Serialize writes to each child session;
continue/repair never races an already running turn. Cancellation propagates to
owned work, and shutdown joins/reaps it. Execution completed != work accepted.

Use a bounded concurrency setting and explicit workspace/permission scope for
children. Do not automatically multiply workers, add orchestration recursion, or
share write access by accident. Parallel conflicting writes must be serialized
or isolated; worktree pools/best-of-N are unnecessary for the first slice.

## 9. Threading — proposed choice

Use standard Tokio and a single owner of each agent's mutable state. Choose
Send-capable public async contracts and owned cross-task messages for new p1 code;
this does not require Arc<Mutex<_>> around all state or a thread per agent.
The initial host can use a current-thread runtime with async I/O and concurrent
agent tasks. CPU-heavy/blocking filesystem/process work must not block that loop;
use bounded blocking work or async subprocess/I/O operations as appropriate.

This is new code, not a requirement to convert all of Iris. Reuse protocol parsers
and algorithms without inheriting global Rc/RefCell ToolState coupling. A private
local executor adapter remains possible if a reused component needs !Send; do not
make !Send a universal public limitation merely because the donor uses it.

Verified locally: Iris's worker service already runs concurrent !Send workers on
Tokio LocalSet/current-thread. Thus !Send is NOT synonymous with "no concurrency".
Iris also has a Send async provider transport path; its providers are not uniformly
blocking. Cancellation of spawn_blocking cannot forcibly stop the underlying work:
blocking transports require their own deadlines/cooperative cancellation and
bounded resources. Prefer native async network transport for new adapters.

## 10. First slice and validation

Implement the boundaries from the start: small contracts/core; Anthropic and
OpenAI route adapters; individually selectable read/edit/patch/shell/search tools;
explicit assembly and family prompt/config files; memory and JSONL stores; simple
terminal/headless host; optional delegation tool + in-process worker service.
Package shared helpers normally. No generic router service is required beyond
explicit model/provider configuration in this slice. Build only supported tool
forms and prompts required for actual Claude/GPT routes.

Internal increments: fake-provider loop; one real route and tools; second route
with different environment; journal/resume semantics; optional child execution and
completion. These increments do not become separate products. Slice acceptance:
- Core builds/tests without provider SDKs, file tools, persistence formats or TUI.
- A real coding task runs on each route with only its intended prompt/tools.
- Replace/remove a tool or provider through composition without editing the loop.
- Both adapters pass shared ordering, partial-call, error and cancellation checks;
  route-specific fixtures cover native declarations/replay and usage differences.
- Memory and file storage preserve committed model-visible state consistently.
- With delegation installed, a child on another route completes and wakes its
  parent; without it, ordinary coding still works. Repair retains the child state.
- Report observed memory, tokens, and available cost; no numerical targets invented.

Iris donates selected logic and tests, not the entire worker/session machinery.
The old brain module-boundaries page contributes dependency direction and separation
of runtime/router/workers. Its default-orchestrator statements are superseded by
pillars rev 2, and its crate list remains a proposal.

## 11. Recovered Iris proposals: what carries over

Read directly: [issue #73](https://github.com/5omeOtherGuy/iris-agent/issues/73)
and [issue #18](https://github.com/5omeOtherGuy/iris-agent/issues/18).

Keep #73's single resolved selection context: provider route, model, thinking
level, and optional future mode, extended with the task/role inputs needed here.
Prompt selection, tool construction, and dispatch consume the SAME resolved
environment. Modules may declare applicability/required capabilities; host
configuration selects among eligible modules. A matching declaration offers a
candidate, not permission to execute. Explicit host policy still governs grants
and per-call authorization. This is compatible with configuration choosing and
tools offering; no decentralized auto-registration is necessary.

Keep the invariant: a non-granted tool is unavailable to dispatch even if a model
invents its name or an old transcript contains a call. Do not inherit Iris's
separate hidden-but-still-runnable registry as the default. Already completed old
tool results are history and need no executable tool. An unresolved old call must
be reconciled against its recorded tool identity and current grant before any
execution; it does not silently resurrect a removed tool. Environment switching
should normally wait until outstanding tool calls are settled.

Leave behind #73's fragment frontmatter/slot machinery and #18's WASM/Extism loader,
override ordering, and plugin policy. Those were Iris proposals, not p1 requirements.
Keep tool input/output as data suitable for a future process/WASM adapter, but do
not require a remote ABI or serialize all internal workspace handles today.

## 12. Remaining bounded technical work

Before coding final wire types, verify exact request/tool/replay shapes against
the TWO chosen actual provider routes and capture fixtures. Decide the minimum
journal schema and sync guarantee, which edit/patch implementations are reused,
and the smallest interactive host surface. No new product interview is needed to
resolve these technical choices; present concrete designs/tradeoffs.
