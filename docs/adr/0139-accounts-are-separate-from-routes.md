---
adr: 139
title: Accounts are separate from routes
status: proposed
date: 2026-10-09
deciders: owner
supersedes: []
superseded_by: []
sources: [https://github.com/5omeOtherGuy/phaseone/issues/634, https://github.com/5omeOtherGuy/phaseone/issues/87, docs/adr/0039-a-provider-is-composed-from-a-wire-adapter-a-route-and-a-model-profile.md, docs/adr/0040-p1-keeps-logins-in-one-file-keyed-by-route-environment-variables-win-other-tools-logins-are-borrowed.md, docs/adr/0110-credential-sources-are-bound-to-endpoint-origins.md, docs/adr/0134-opencode-go-deepseek-over-messages.md, docs/design/routes-and-profiles.md, docs/design/credentials.md, crates/p1-host/src/routes.rs, crates/p1-host/src/usage.rs, crates/p1-auth/src/store.rs, crates/p1-usage/src/probe.rs]
---
# ADR-0139: Accounts are separate from routes

## Context

ADR-0039 composes a provider from a wire adapter, a "route/account" and a model profile, and
keeps route and account in one file (`routes/<id>.toml` with an inline `[credential]`).
ADR-0040 keys p1's store by route id. Owner direction 2026-10-09 (issue #634): make the account
an object of its own so any number of accounts combine with any number of routes, and "Design
it so anybody, with any setup can use it".

Consequences of the merged object, checked at `a250b52`:
- One route file per account of the same provider, differing only in `id`, `origin_route` and
  `[credential]`: `opencode-go-subscription` and `opencode-go-{1,2,3}-subscription`, `cline-pass-{1,2}`,
  `opencode-zen-{free,1,2,3}`, `anthropic-subscription{,-2}`.
- One environment per such route, differing only in `route` and a comment, with a byte-identical
  `prompt.md`: `deepseek{,1,2,3}`, `cline{,2}`, `zen{,2,3}`, `claude{,2}`.
- A second wire for an existing account borrows the first route's store entry through
  `credential_route` (ADR-0134: `opencode-go-messages{,-1,-2,-3}`), so the first route stays the
  login's owner.
- `p1 usage` labels and probe selection are hard-coded tables keyed by route id
  (`crates/p1-host/src/usage.rs` `label`, `crates/p1-usage/src/probe.rs` `Shape::of`), and the
  probe reads the store under the route id, not under the `credential_route` identity.
- A user environment cannot use a shipped profile: assembly loads profiles only beside the
  selected environment (issue #87). Routes already resolve across directories.
- `[adapter_settings] account` (Messages: `claude-code-subscription`, `opencode-go`; Responses:
  `codex-subscription`) means a protocol variant, not an account.

## Decision

### 1. Three objects, each defined once

| file | holds | named by |
|---|---|---|
| `profiles/<id>.toml` | what a model is (ADR-0039, unchanged) | environment `profile` |
| `routes/<id>.toml` | how p1 talks to a provider: `adapter`, `endpoint`, `origin_route`, `retry_policy`, `[headers]`, `[adapter_settings]`, `[models.<profile>]`, optional default `account` | environment `route` |
| `accounts/<id>.toml` | who is billed and how p1 authenticates: credential reference, approved endpoint origins, store identity, usage probe, display label | environment `account`, `--account`, `@account`, route `account` |

```toml
# accounts/opencode-go-2.toml
id       = "opencode-go-2"                    # equals the file stem
label    = "opencode go 2"                    # optional; p1 login --list, p1 usage, p1 models
origins  = ["https://opencode.ai"]            # endpoint origins this account's credential may reach
store_id = "opencode-go-2-subscription"       # optional; p1 store entry name, default = id
usage    = "opencode-go"                      # optional; one of the compiled usage probes

[credential]                                  # a reference, never a value
method     = "api-key"                        # api-key | claude-code-oauth | codex-oauth | none
env        = "OPENCODE_GO_2_API_KEY"
store_only = true
# borrow = [...], login_dir = "~/.claude-2"   # as today (ADR-0040, ADR-0061, ADR-0074)

# Compatibility tables (§6, §7); only converted shipped accounts need them.
[legacy_routes]                               # old route id → the route it now means with this account
"opencode-go-2-subscription" = "opencode-go-subscription"
"opencode-go-messages-2"     = "opencode-go-messages"
[legacy_origins]                              # route id → origin string sessions recorded before ADR-0139
"opencode-go-subscription" = "openai-chat/opencode-go-2-subscription"
"opencode-go-messages"     = "anthropic-messages/opencode-go-messages-2"
```

- The `[credential]` table is today's `CredentialSpec` with its validation (ADR-0061, ADR-0070,
  ADR-0074) unchanged. The credential type is spelled `method`; `kind` stays accepted as a serde
  alias of the same field, in route and account files alike. Store-only versus borrowed stays the
  `store_only`/`borrow` property of the account. A new credential method later is a new `method`
  value and touches no route.
- `origins` is required and non-empty for every method (for `none` it only states where the
  account is meant to be used; nothing is sent). Each entry is a `scheme://authority` origin.
- `usage` names a compiled probe; absent means the OAuth methods keep their method's probe and
  every other account has none (`Unsupported`, as today). A probe's URL origin is fixed in code;
  it runs only when that origin is approved for the account under ADR-0110 (§2). A shipped
  account's probe stays compiled with it.
- `store_id` is unique among loaded accounts (the deprecated `credential_route`, §6, excepted).
- An account holds no secret. Masking (ADR-0068, ADR-0108) is unchanged.

### 2. Many-to-many, validated at assembly

An environment selects a route, a profile and an account. Assembly resolves
profile × route × account and refuses, before any credential lookup or request:
- a profile the route does not bind (unchanged);
- a route whose endpoint origin is not in the account's `origins`.

An adapter refuses a credential method it cannot send exactly as today; this ADR adds no
method × adapter table.

ADR-0110's runtime binding stays; its compiled anchor and its keys move from routes to accounts:
- `build.rs` compiles shipped `accounts/*.toml` (id, `store_id`, method, origins, probe) and
  shipped routes (id, every legacy route id, endpoint origin).
- A credential is bound by its store identity. When an account's `store_id` or id equals a shipped
  account's `store_id` or id, or a shipped route or legacy route id, the credential may reach only
  that shipped account's compiled origins, whichever file names it; trust cannot extend it.
- Borrowed methods reach only compiled shipped origins of the same method.
- Any other account needs each origin it uses, chat or usage probe, approved in p1's protected
  store, checked before every access and refresh. An approval of one origin never covers another
  (a usage host does not inherit its chat host's approval).
- Loopback endpoints (`127.0.0.1`, `::1`, `localhost`) need no approval record, and `none` sends
  nothing, as today.
- Declaring an origin in a file never approves it.

### 3. Selection

The first rule that applies picks the account:
1. `--account <id>` on the command line. It applies to the run's own model selection; model
   references inside settings, workflow roles, fallback chains and subagent lists keep their
   own `@account`. `--account x` together with `--model …@y` and `x ≠ y` is an error.
2. `@<account>` at the end of a model reference: `environment/profile[:effort][@account]`,
   accepted wherever a model reference is (`--model`, `settings.toml`, workflow roles and their
   `fallback`, subagent `models`, the TUI `/model`).
3. The environment's `account = "<id>"`, or the account implied by the legacy route id the
   environment names (§6); both present and different is a load error.
4. The route's `account = "<id>"` (its default).
5. The route's inline `[credential]` (implicit account, §6).
6. The only loaded account whose `origins` cover the route's endpoint. With none or several,
   assembly fails and lists the candidates.

A single-account setup stays one file: either a route with its inline `[credential]`, or one
account file and no account line anywhere.

### 4. Shipped and user files compose

Every kind is found as `<dir>/../<kind>` for each directory of the list environments already use
(`crates/p1-host/src/main.rs`): `$P1_CONFIG_DIR/environments` (default
`~/.config/p1/environments`) first, then `$P1_ENVIRONMENTS_DIR` if set, else the shipped
`<exe>/../share/p1/environments` (and the repository's in debug builds). `accounts/`, `routes/`
and `profiles/` all follow it; profiles stop being read only beside the selected environment
(closes #87's profile part). The first file for an id wins whole and files never merge; two files
for one id in the same directory are a load error. A user profile with a shipped id therefore
changes the shipped environments that use it, as a user route already does today. `p1 env show`
names the file each environment, route, profile and account came from. A user file that reuses
a shipped id keeps the compiled checks of §2. p1's store stays at `$XDG_CONFIG_HOME/p1/auth.json`
(ADR-0040).

### 5. Store and operator commands work on accounts

- The store keys entries and their origin approvals by the account's `store_id` (default its id).
  An approval record may hold several origins; a legacy single-origin record reads as one.
- `p1 login <account>`, `--trust-endpoint`, `--from-claude-code`, `p1 logout <account>` act on
  the account. Login and trust approve the account's declared origins that §2 lets them approve,
  and print each one. An id is looked up as an account first, then as a route or legacy route id,
  which resolves to the account rules 3–5 pick for it; the output names that account.
- `p1 login --list` and `p1 usage` show one row per account (label, method, source line, origins;
  the routes that use it), not one per route. `p1 models` shows `E/P[@account]` rows with the
  account column. The hard-coded label and probe tables move into the shipped account files.

### 6. Compatibility: nothing changes for an existing setup

- **Implicit account.** A route with an inline `[credential]` (today's form) is a route plus an
  account whose id, `store_id` and label are the route id, whose `origins` are the route's
  endpoint origin, and whose credential is that table. It can be named by id like any account.
  A route with both an inline `[credential]` and `account` is a load error. An implicit account
  takes part in the same first-file-wins order as account files: a user route copy whose id
  equals a shipped account id shadows that account, and the compiled checks of §2 still bind it.
- **`credential_route`** stays accepted with today's meaning, deprecated: the route's implicit
  account takes its `store_id` and origin approval from the account of the named route or legacy
  route id, keeps its own `env`, and keeps today's limits (store-only `api-key`, shipped target,
  same origin). A later ADR removes it; the shipped routes stop using it (§9).
- **Legacy route ids.** An account's `[legacy_routes]` maps an old route id to the route it now
  means with this account. It resolves wherever a route id does (environment `route`,
  `credential_route`, `p1 login`). The table lives in the account, so a user copy of the
  canonical route keeps it; a user file with the old id itself wins, as any user file does.
- **Environment aliases.** An environment directory may hold only an `environment.toml` with
  `alias_of = "<environment>"` and `account = "<id>"`, and no `prompt.md`: the named environment
  with that account. `deepseek2/deepseek-v4.1-flash` keeps working wherever it is written.
  Listings show canonical references (`deepseek/deepseek-v4.1-flash@opencode-go-2`).
- **Stored logins and variables.** Converted shipped accounts set `store_id` to the old route id
  and keep the old `env` names, so every store entry, origin approval and environment variable is
  found where it is. Nothing in `auth.json` is rewritten.
- **`[adapter_settings] account`** is renamed (§8); the old spelling is accepted with a load
  warning shown by `p1 env show`; both spellings together are an error.

### 7. Replay origin, resume and failover

- `Origin.route` belongs to the route × account pair: the route's `origin_route` for its implicit
  account; for any other account the account's `[legacy_origins]` entry for that route id if
  present, otherwise `<origin_route>@<account id>`. `[legacy_origins]` holds the origin string
  sessions recorded before this ADR, so converted pairs keep their exact identity. It lives in the
  account, so a user copy of the canonical route keeps it.
- Two pairs that resolve to one origin string must have the same adapter, endpoint origin and
  store identity (a user copy of an old route is such a pair); otherwise assembling either pair
  fails. This machine-checks the rule of `routes-and-profiles.md` §1.2.
- **Owner decision 2026-10-09:** a session may continue on another account of the same route
  (resume, `/model`, fallback). Because the account is part of the origin, this is an origin
  change under ADR-0049: the conversation and tool history carry, and opaque reasoning and
  signatures from the other account are dropped. A provider never receives another account's
  reasoning tokens.
- Failover is unchanged in mechanism (ADR-0054, ADR-0074): an exhausted account is a route
  failure, and a chain link may now name another account of the same route
  (`deepseek/deepseek-v4.1-flash@opencode-go-3`) as well as another route.

### 8. Renamed adapter setting

**Owner decision 2026-10-09:** `[adapter_settings] account` becomes `dialect` on the Messages and
Responses adapters, with unchanged values. The host validates the route file under either
spelling and hands the provider component only `dialect`. The Chat adapter already uses `dialect` for the same
idea: a compiled, finite set of variants of one wire protocol. From here on, "account" names
only the object of §1.

### 9. Shipped data

**Owner decision 2026-10-09:** convert both the per-account routes and the per-account
environments, with the old names kept as aliases.
- Routes that differ only in `id`, `origin_route`, `[credential]` and `credential_route` collapse
  into one route per wire. Every former credential becomes an account file whose `store_id`,
  `env`, `label`, `usage`, `[legacy_routes]` and `[legacy_origins]` preserve today's values, so
  every former per-account route id stays resolvable. The implementer verifies each group by diff; a file that
  differs in anything else stays its own route.
- Environments that differ only in `route` and comments collapse into one; each former copy
  becomes an environment alias (§6).
- Every shipped environment, workflow default and subagent chain resolves to the same route,
  credential source, origin string, prompt and tools as before.

Expected result, to be confirmed by the phase-5 diff:

| route (canonical id) | accounts (default first) | legacy route ids |
|---|---|---|
| `anthropic-subscription` | `claude`, `claude-2` | `anthropic-subscription-2` |
| `openai-codex-subscription` | `codex` | |
| `glm-subscription` | `zai` | |
| `kimi-coding-subscription` | `kimi` | |
| `opencode-go-subscription` | `opencode-go`, `opencode-go-1`, `-2`, `-3` | `opencode-go-{1,2,3}-subscription` |
| `opencode-go-messages` | the same four Go accounts | `opencode-go-messages-{1,2,3}` |
| `cline-pass` | `cline-pass-1`, `cline-pass-2` | `cline-pass-1`, `cline-pass-2` |
| `opencode-zen` | `opencode-zen-1`, `-2`, `-3`, `opencode-zen-free` | `opencode-zen-{1,2,3}`, `opencode-zen-free` |

Environment aliases: `deepseek{1,2,3}` → `deepseek`, `cline2` → `cline`, `zen{2,3}` → `zen`,
`claude2` → `claude`.

### 10. Order of work and out of scope

Phase 1 is this ADR alone; the owner accepts it before phases 2–5 start (#634): 2 account files,
selection and assembly validation; 3 store and commands; 4 compatibility (§6, §7); 5 shipped data
(§9). Not in this decision: billing math, spending limits, rate limiting, new credential methods,
UI beyond the existing commands, discovery of other tools' accounts beyond `borrow`, and removal
of the deprecated aliases.

## Consequences

- One key, several keys of one provider, one subscription over several wires or hosts,
  aggregators, keyless local servers and credential-injecting proxies are each expressed without
  copying a file; adding an account or a wire is one small file.
- ADR-0039 is amended: the route no longer holds the account. ADR-0040 is amended: the store is
  keyed by account (`store_id`), with precedence and location unchanged. ADR-0134's
  `credential_route` is deprecated in favour of naming an account. ADR-0110's checks are kept and
  re-keyed from route to account.
- `auth.json.origins` may hold a list per entry; an older p1 binary then refuses the whole
  origins file, not only that record (downgrade is not supported).
- The provider components receive `dialect` instead of `account`; the module set is rebuilt.
- Legacy route ids, environment aliases, `credential_route` and the old setting name are
  maintained until a later ADR removes them.
- Automatic selection (rule 6) can turn ambiguous when a user adds a second compatible account;
  assembly then fails with the candidate list rather than guessing. Shipped routes name a default
  account, so they never depend on rule 6.
- The latent probe defect (store read under the route id, not the store identity) disappears,
  because the probe reads the account's `store_id`.

## Alternatives considered

- **Keep per-account route copies** (today): every account multiplies route and environment files,
  and a second wire needs `credential_route`.
- **Account inside the environment file**: still duplicates the credential per environment and
  gives the store no identity of its own.
- **Account excluded from the replay origin** (keep reasoning across accounts): rejected by the
  owner; providers may bind opaque reasoning to an account (unverified), and a rejection would
  stop a session in the middle of a failover.
- **Refuse resume across accounts**: would remove today's working `claude` → `claude2` switch.
- **Rewrite store entries under new account ids**: writes to the credential store for no gain;
  `store_id` finds them in place.
- **Approve origins from the account file**: would let a file approve itself (ADR-0110).
- **Other names for the renamed setting** (`service`, `client`): a second word beside `dialect`
  for the same concept, or misleading for `opencode-go`.

## Evidence

Code facts at `a250b52`: `RouteFile` and `credential_route_id` (`crates/p1-host/src/routes.rs`),
`CredentialSpec` and `CredentialKind` (`crates/p1-auth/src/spec.rs`), store keys and
`auth.json.origins` (`crates/p1-auth/src/store.rs`), compiled shipped table
(`crates/p1-host/build.rs`), usage labels and probe shapes (`crates/p1-host/src/usage.rs`,
`crates/p1-usage/src/probe.rs`), profile lookup beside the environment
(`crates/p1-assembly/src/lib.rs` `load_profile`), replay origin drop on foreign origin (each
adapter's `replay.rs`). Shipped duplication: `diff` of the route and environment groups listed in
Context shows only the fields named there. Owner decisions: question dialog 2026-10-09 (resume
across accounts, conversion of routes and environments, `dialect`), recorded as D39.
