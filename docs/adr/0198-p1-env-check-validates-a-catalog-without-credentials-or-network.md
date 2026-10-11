---
adr: 198
title: p1 env check validates a catalog without credentials or network
status: accepted
date: 2026-10-11
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [https://github.com/5omeOtherGuy/phaseone/issues/840, https://github.com/5omeOtherGuy/phaseone/issues/846]
---
# ADR-0198: p1 env check validates a catalog without credentials or network

## Context

The owner split of 2026-10-11 (epic #840, ADR-0200) separates the bare product,
subscription connectors and the owner's catalog pack. Pack-only pull requests
must be checkable against the released binary without building Rust. The command
surface and credential-free checks were specified in #846; this record's number
was reserved on #766.

Today `p1 env show NAME` assembles one environment without provider access, and
`p1 models` enumerates routed profiles without credential resolution. Content
checks of the shipped catalog live in Rust tests tied to this repository.

## Decision

Add `p1 env check [--dir CATALOG_ROOT]... [--json]`. Without explicit directories,
it checks the host's merged search view. Explicit catalog roots contain
`environments/` and its siblings and layer, first argument first, over the shipped
catalog rather than over ambient user overrides. Names and aliases resolve by the
same first-match rule as a run.

Check each environment's declared default and every profile its route binds with
the real inspection catalog used by `env show`. Loading and assembly validate
prompt placeholders, assembled tool references, routes, account origins, profiles,
provider options, context, tool concurrency, capabilities, module locks and MCP
syntax. Resolve configured subagent environments, prompt files, tools and model
references without starting a child. `subagents.toml` lives beside `environments/`;
the older `environments/subagents.toml` spelling remains a per-layer fallback.

Inspection never reads credential content from `auth.json` or borrowed login files,
accesses a provider,
dispatches a tool, connects an MCP server,
starts a module process or resolves a credential value. Missing credentials and
login-dependent origin approval are not catalog failures, matching `env show`;
static account, compiled shipped-origin and borrowed-kind origin rules still apply.
Catalog construction sees only non-secret location variables so eager GitHub token
lookup cannot read a key during inspection. Components may be
loaded and assembled in-process, so checking a catalog verifies it against the
modules actually shipped by the installed p1.

Text prints one result per environment and counts; exit zero means every result
passed. JSON prints only a sorted array of results with `environment`, `file`,
`profiles`, `ok` and `failures`; counts are derivable. Catalog-wide errors attach
to the affected results; a dangling subagent contributes a failed result naming
its missing environment. Error diagnostics identify the source file.

## Consequences

- Pack-only CI can run the installed binary, with no Rust build or live login.
- Generic catalog-content checks can leave the Rust shipped-content suites later:
  assembly, route/profile consistency, aliases, reference resolution and table
  validation. This change does not delete or weaken any existing test.
- Owner-specific expectations (exact model rows, tool grants, prompt wording,
  context budgets and effort choices) remain pack checks, not product policy.
  Exhaustive prompt-tool subset sweeps and literal-name coherence are not added
  to the generic command: a valid prompt need not mention every assembled tool.
- Rust tests still pin parser and assembly semantics, origin refusal order,
  module boundaries, no credential reads and the new command's public contract.
- A valid catalog says nothing about login availability, remote endpoints, MCP
  discovery or module execution; those require a real run.

## Alternatives considered

- `cargo test` pinned to the catalog's checkout: requires a compiler and couples
  pack data to the source tree instead of the released binary.
- `p1 models` only: enumerates bindings but does not assemble all prompts and
  tools, include every companion environment or validate subagent references.
- Separate validation schemas or fake inspection providers: duplicate runtime
  rules and could approve a catalog the installed host cannot assemble.

## Evidence

`crates/p1-host/src/run.rs` factors the inert catalog from `env show` without
changing its output. `models.rs` shares per-environment profile enumeration.
`crates/p1-assembly/src/lib.rs` validates context, capabilities and tool
concurrency at load and invokes provider validation at assembly;
`load_mcp_servers` and `load_modules_lock` perform non-executing resolution.
`crates/p1-host/tests/env_check.rs` exercises clean and broken tempfile catalogs,
all routed profiles, aliases, explicit layering, MCP non-execution and a poisoned
credential-file path modeled on `credential_origins.rs`; on Linux with strace
installed, a system-call trace checks successful validation and refusal against
unreadable sentinel credential documents: no credential document opens for reading,
no sentinel appears in output and no network connection occurs. The existing
protected reader opens metadata-only `O_PATH` handles to check credential identities;
these cannot read content. This command preserves that credential-refusal policy,
rather than changing file-tool security as part of catalog inspection.
Route and account loading uses the existing bounded credential-refusing reader,
so catalog links cannot bypass the inspection invariant.

Verification commands: `cargo test -p p1-host --test env_check`,
`cargo run -q -p p1-host -- env check` with an empty scratch `P1_CONFIG_DIR`,
`python3 scripts/adr.py check`, and `scripts/pre-push.sh`.
