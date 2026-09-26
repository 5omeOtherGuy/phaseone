# Adapters: the generic tool adapter

Status: published freeze item 12 of the WebAssembly boundary (ADR-0071): the generic tool
adapter in `p1-module-runtime`. Streams S1–S6 implement tool **components**, never tool
adapters: every tool module reaches the core through the one adapter below. The code is
[`crates/p1-module-runtime/src/tool.rs`](../../../crates/p1-module-runtime/src/tool.rs); the
execution model it sits on is [ADR-0082](../../adr/0082-component-abi-and-execution-ownership.md)
and [`cancellation.md`](cancellation.md).

## `WasmTool`

`WasmTool` implements `p1_contracts::Tool` over one loaded module of the `tool` class
(`LoadedModule` from the [loader](package.md#the-loader-freeze-item-6)). The only constructor is

```rust
pub fn wasm_tool(module: &LoadedModule, services: Services, limits: ExecutionLimits,
                 counter: &Arc<MaskCounter>) -> Result<Arc<dyn Tool>, ToolError>;
```

and it must run inside a Tokio runtime, which runs the tool's executor. Construction refuses a
module of another class (`ToolError::NotATool`), a grant whose service the caller did not pass
(`ToolError::Link`), a component that does not fit the `tool` world as linked
(`ToolError::Instantiate`) and a module whose `declaration` traps or is invalid
(`ToolError::Declaration`).

| `Tool` method | How the adapter answers it |
|---|---|
| `declaration` | read once at construction through the restricted path and cached |
| `identity` | the loader-built `ToolIdentity` (manifest `name` and `variant`); the `tool` world exports none, so a module cannot claim one ([`package.md`](package.md#the-loader-built-toolidentity)) |
| `effect` | the `effect` export on the restricted path; anything unreadable is `Effect::Executes` |
| `describe` | the `describe` export on the restricted path, parsed as a `call-description`; a failure is the empty description with the verb `call` |
| `describe_result` | the `describe-result` export on the restricted path; a failure is the host's own first-line summary |
| `execute` | the `execute` export on the executor: a fresh Store and instance per call, `ToolContext.cancel` honoured, the per-call fuel and deadline of `ExecutionLimits`, and every failure mapped through `ModuleFailure` into a `ToolOutcome` |

**Inspection on the restricted path.** `effect`, `describe` and `describe_result` are
synchronous and run on a second instance with no capability linked, on the caller's own thread,
under a tight fuel budget ([`restricted.rs`](../../../crates/p1-module-runtime/src/restricted.rs);
[`cancellation.md`](cancellation.md#the-restricted-path-f10)).

**Per-call execution.** `execute` sends the call to the module's one Store-owning executor over a
channel and awaits the reply, so it is a boxed `Send` future on either Tokio flavour (ADR-0015);
only the capabilities the manifest grants are linked, from the `Services` the caller passes
([`executor.rs`](../../../crates/p1-module-runtime/src/executor.rs),
[`capabilities.rs`](../../../crates/p1-module-runtime/src/capabilities.rs)). Values cross as the
JSON families of [`protocol.md`](protocol.md): the call as `tool-call`, the answer as
`tool-outcome`; text that does not parse is `ModuleFailure::InvalidOutput`.

**Wrapped in `RedactingTool`.** `wasm_tool` returns the adapter already wrapped by
`p1_redact::redacted` with the caller's `MaskCounter`; an unwrapped `WasmTool` is never handed
out, so no module output reaches history, the journal or a summary unmasked (ADR-0068).

## Exercised by the fixture

The fixture component `p1/fixture`
([`modules/p1-module-fixture/`](../../../modules/p1-module-fixture/)) implements the `tool` world
with one mode per runtime property. The adapter is tested over it in
[`runtime_spike.rs`](../../../crates/p1-module-tests/tests/runtime_spike.rs) — loading, the
restricted path (`inspection_is_synchronous_inside_a_running_task`), per-call instances,
asynchronous imports, concurrent calls and `the_host_gets_the_redacting_wrapper` — and in
[`cancellation.rs`](../../../crates/p1-module-tests/tests/cancellation.rs), each case on a
current-thread and a multi-thread Tokio runtime. The adapter landed with S0.5 (PR #252, merge
commit `4872fd78`).

## The other adapters

Each contract gets one generic adapter in `p1-module-runtime`, each in its own file of
`crates/p1-module-runtime/src/` named in the owning stream's brief, built on the same loader,
executor and restricted path, on the streaming-resource contract of item 10
([`wit.md`](wit.md#streaming-resources-freeze-item-10)) and on the frozen worlds of
[`wit.md`](wit.md#worlds-freeze-item-1):

| Adapter | Contract | World | Owner |
|---|---|---|---|
| `WasmTool` | `p1_contracts::Tool` | `tool` | S0 (this document) |
| `WasmProvider` | `p1_contracts::Provider` | `provider` | S4: one Store-owning executor behind a channel, boxed `Send` futures (ADR-0015); the transport broker sends (freeze item 9, [`wit.md`](wit.md#transport-retry-backoff-and-the-one-refresh-stay-native-freeze-item-9)) |
| the context policy adapter | `p1_contracts::ContextPolicy` | `context-policy` | S5 |
| the authorization policy adapter | `p1_contracts::AuthorizationPolicy` | `authorization-policy` | S5, through the native ask bridge |

None of these three exists yet; this table fixes where each lives and what it is built on, not
its code.
