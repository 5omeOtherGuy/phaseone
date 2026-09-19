# p1 — proposed design in one page (Fable + Astra; for owner review, NOT approved)

Detail and reasoning: `seams.md` (v1). Product direction: `pillars.md` (rev 2).

## The shape

```
            frontend / host  (terminal, headless; wires everything together)
                     │
        environment assembly  ── reads: model route + task + your configuration
          │        │        │
     provider    tools    policies (context preparation, execution authorization)
          └────────┼────────┘
               agent core  ── one agent: request → stream → tool calls → repeat
                     │
              session journal (memory or JSONL)
   optional: delegation tool ──► worker service ──► more agents, built by the same assembly
```

1. **Module** = an independently selectable implementation behind an explicit Rust
   interface, normally its own crate. Composed with ordinary constructors at one
   composition root. No plugin loader, no service locator, no global registry.
   Every tool is its own module; tools may share small helper libraries.
2. **Agent core** knows only contracts: it runs one agent's loop, handles
   cancellation and steering, emits events. It never names a provider, tool, file
   format, model-specific prompt template or UI. It accepts prompt text as input
   and builds/tests without concrete provider, tool, persistence or frontend modules.
3. **Provider** = translation only: request in, stream out, plus honest capability
   reporting for a concrete model + route. Strict rules (one terminal outcome, partial
   tool calls never executed, unknown usage is not zero, unsupported options are
   errors, native replay data is opaque + versioned + tagged with its origin).
4. **Tool** = declaration + validation + execution, registered together under one
   call name so generated tool descriptions correspond to the executable interface;
   tests check that hand-authored prompt prose agrees too. Safety
   invariants (path confinement, atomic writes) stay inside the tool; permission
   policy stays outside it. Variant rule: shared implementation when semantics are
   shared, a wrapper when only the description/format differs, a separate tool when
   behaviour differs (edit vs apply_patch may share a small mutation library).
5. **Environment assembly — where the harness reshapes itself.** One resolved
   selection (model route, thinking level, task/role, later mode) drives BOTH the
   prompt and the exact tool set — the idea from Iris issue #73, without its fragment
   machinery. Tools offer, configuration chooses, providers validate. An agent owns
   only what was assembled for it; anything else does not exist for it and cannot be
   dispatched. Whole prompt files per model family to start. Bad combinations fail
   before the run starts. Delegated agents are built by the same assembly.
6. **Policies, not a hook platform.** Two named decision points to start: preparing
   the context before each request, and authorizing execution before side effects.
   A turn-completion policy and after-tool interception are added only when a real
   consumer needs them. Observation is via events.
7. **Session journal.** One small append-only record of what the model was actually
   sent and returned (incl. effective prompt/environment and replay data); in-memory
   state is its projection, not a second truth. Memory and JSONL backends. Commits at
   meaningful boundaries; a crash between a side effect and its recorded result is
   marked unknown and reconciled, never blindly re-run.
8. **Delegation is optional.** A tool that talks to a worker service: start, wait,
   be woken on completion, cancel, continue the same child for repair. Completion is
   retained state plus a notification, so a missed wake-up is recoverable. Without the
   module installed, the harness is a plain coding agent.
9. **Threading.** New p1 code uses standard Tokio with Send-capable public interfaces
   and one owner per agent's state; donor Iris code that is not Send can sit behind a
   private adapter. To be confirmed by a small spike before extraction.

## Tradeoffs we are choosing (say if you disagree)

- **Compile-time modules over runtime plugins** — far cheaper and leaner; cost: adding
  a module means rebuilding. WASM/out-of-process tools (Iris #18) stay possible later
  because tool input/output is plain data.
- **Configuration chooses tools per model** over tools auto-registering themselves —
  one inspectable place per model family; cost: a new tool must be named in config.
- **Journal as the single truth** over Pi-style mirror — better resume/replay for long
  runs; cost: the core owns a small commit interface.
- **Send-capable interfaces** over inheriting Iris's local-task restriction —
  freer embedding and the option to schedule across threads; cost: some donor code
  needs adapting. Concurrent agents also work on a single thread with async I/O.
- **Few named policy points** over a general hook bus — smaller core; cost: a new kind
  of interception means a contract change.

## First slice (already modular)

Contracts + core · Anthropic and OpenAI route adapters · separate read / edit / patch /
shell / search tools · assembly with Claude and GPT prompt+config files · memory and
JSONL journal · simple terminal/headless host · optional delegation tool + in-process
worker service. Accepted when: the core builds without concrete provider/tool/store/UI
modules; a real coding task
runs on each route with only its own prompt and tools; a tool or provider can be
swapped without touching the loop; both adapters pass the same stream/cancel/error
checks; a delegated child on the other route completes and wakes its parent — and the
harness still works with delegation removed.

## Next bounded step

Verify the exact request/tool/replay shapes of the two real routes and capture
fixtures; try a minimal new loop with Send interfaces and representative donor
components (not a wholesale Iris conversion); then settle the contract types.
