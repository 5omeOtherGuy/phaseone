# Credentials — crate `p1-auth` (ADR-0040, step 5 of ADR-0039; self-contained routes, ADR-0061)

Status: spec. Credentials belong to the ROUTE. After the provider split the lookup code still
sits in three places (`p1-provider-anthropic/src/credentials.rs`, `p1-provider-openai/src/credentials.rs`,
`p1-host/src/auth.rs`); this step moves it into one crate and adds the precedence chain and the
"which source" report. Borrowing stays the default; §6 adds `p1 login` for pasted API keys (ADR-0044).
§8 adds the opt-in `store_only` policy (ADR-0061): every SHIPPED route is now self-contained and
reads no other tool's login at runtime, and minting an independent OAuth grant is the remaining work.

## 1. Crate and dependencies

`crates/p1-auth` depends on `p1-contracts` and `p1-provider-http` (`CredentialSource`,
`Credential`, `Transport` for OAuth refresh, `lock_exclusive`, the atomic writer). It names no
wire adapter, no model profile and no host type. The adapters lose their `credentials` modules
and no longer know where a credential comes from; they keep receiving `Arc<dyn CredentialSource>`.

## 2. What a route says

The route file's `[credential]` table deserializes into `p1_auth::CredentialSpec`
(`deny_unknown_fields`; the host's `CredentialRef` is replaced by it, file format unchanged):

| kind | fields | sources tried, in order |
|---|---|---|
| `api-key` | `env` (required name), `borrow` (ordered list of `opencode:<key>` / `pi:<key>`) | env var → p1 store entry for the route → each borrowed login |
| `claude-code-oauth` | `env` optional | env var (a bearer token, no refresh) → p1 store → Claude Code's login file |
| `codex-oauth` | `env` optional | env var → p1 store → Codex CLI's login file |
| `none` | none | NOTHING. An egress proxy injects the provider's credential after the request leaves the process, so no variable, store entry or login is read, and the adapter sends no authentication header (§9) |

`store_only` (boolean, default `false`) is the one policy field on the table, for every kind
(ADR-0061, §8). Written, the chain stops after p1's own store: the documented variable and the
store entry, and no other tool's login. Absent, the rows above are unchanged.

`kind = "none"` is not a policy but a KIND with no source at all (issue #134, §9). Naming a
source beside it is a contradiction, so `env`, a nonempty `borrow` or `store_only = true` on a
`none` route is a load error.

`p1_auth::resolve(route_id, &spec, transport, &Locations) -> Arc<dyn CredentialSource>`.
`Locations` carries every directory the chain may touch (home, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`,
`PI_CODING_AGENT_DIR`, the process environment as a lookup function) so that tests never read
the real home or the real environment; `Locations::from_process()` is the production value.

## 3. The chain

- Access is LAZY and re-evaluated on every `access()`: a key rotated in a file, or a variable that
  appears, is picked up without a restart (today's behaviour of the borrowed key source).
- The first source that HAS an entry wins. A source that has an entry which is unusable (wrong
  type, malformed file, command-backed key `!…`) is an ERROR naming that source — not a silent
  fall-through to the next one: a broken explicit choice must not be papered over.
- **p1 store** (`$XDG_CONFIG_HOME/p1/auth.json`, else `~/.config/p1/auth.json`): READ only in
  this step. One JSON object keyed by route id; entries `{"type":"api_key","key":…}` or
  `{"type":"oauth","access":…,"refresh":…,"expires":…,"account_id":…}`. A store file or
  directory that is group/world-accessible is refused with a message that says `chmod 600`/`700`
  (the borrowed files of other tools are read as they are — their permissions are theirs).
  An `oauth` entry in p1's store is refreshed and written back to p1's store (same lock, same
  atomic writer as the borrowed OAuth sources) — that is "write-back to the source", not a login.
- **Refresh writes back to the source the credential came from, never to another one** (ADR-0040).
  An env-var credential is never refreshed: `refresh()` re-reads the chain and fails with a
  message naming the variable if the value is unchanged.
- `refresh(rejected)`: unchanged semantics — a different current credential is returned, the
  same one is an authentication error whose message names the SOURCE (never the value) and
  what to do.
- A `store_only` route never CONSTRUCTS a borrowed source (§8): the CLI's login is not in the
  chain, so it is not opened even when the store is absent, unusable or has just been rejected.
  Absent means an error that names p1's store; the CLI login is not a fall-through.

## 4. Which source — visible, never the value

`CredentialSource` gains one defaulted method, `proxy_injected()` (§9): `false` for every source
that resolves a credential of its own, `true` for `kind = "none"`. `p1_auth::describe(route_id,
&spec, &Locations) -> SourceReport` is a separate, non-secret probe:
`{ chosen: Option<SourceName>, tried: Vec<(SourceName, Presence)>, policy: CredentialPolicy }`
with `SourceName` rendering as `env OPENCODE_API_KEY`, `p1 store`, `opencode login`, `pi login`,
`Claude Code login`, `Codex login`, and `Presence` = `present | absent | unusable(<reason>)`.
`policy` is `Chain` (the default), `StoreOnly` or `ProxyInjected` (`kind = "none"`, §9).
`SourceReport::line()` returns the chosen source (or `none — <what to do>`), then ` [p1 store only]`
when the policy is `StoreOnly`; a `ProxyInjected` route's line is `none (proxy-injected) — the
egress proxy injects the credential`, and its `tried` list is empty. The setting is visible in
`p1 env show`, `p1 login --list` and `p1 models` without reading any credential; `p1 login --list`
also spells the KIND as `none (proxy-injected)` (`CredentialKind::label`).
`p1 env show` prints it as `credential  <line>`. The report reads files to see whether an entry
EXISTS; it never returns, logs or formats a credential value, and its `Debug` is redacted by
construction (there is no field that could hold one).

## 5. Must-pass

a. Precedence for each kind: env beats store beats borrowed; absent sources are skipped; an
   unusable earlier source is an error, not a skip.
b. Rotation: a changed file value / a newly set variable is seen by the next `access()`.
c. Write-back: a refreshed borrowed OAuth token lands in the file it came from; a refreshed p1
   store entry lands in p1's store; nothing else in either file changes (byte-compare the rest);
   the two existing refresh-contention tests move with the code and pass unchanged.
d. Store permissions: 0644 file or 0755 directory → refused with the chmod message; 0600/0700 ok.
e. `describe` never contains a value (seed every source with a sentinel and assert its absence in
   `Debug`, `Display` and the `env show` output) and names the chosen source correctly for every
   row of (a).
f. No test touches the real home or the real process environment.
g. Behaviour on the wire is unchanged: every adapter's characterization and conformance test
   passes with its expectations untouched.
h. `store_only` (§8): the borrowed source is not in `tried`, the line carries the marker, and a
   store-only chain with an absent or rejected p1 entry fails naming p1's store without opening
   the CLI's login path at all; the same chain WITHOUT the field still reads and names it.
i. Every shipped `routes/*.toml` sets `store_only = true` and lists no nonempty `borrow`.

## 6. `p1 login` — pasted keys into p1's own store (ADR-0044)

Owner, 2026-09-20: "Keep the store. You can dispatch a worker to work on the login
implementation." Scope: API KEYS. Browser/OAuth logins stay borrowed from the official tools.

```
p1 login <route>          read one key, store it for that route
p1 login --list           every route: its credential kind and the source report of §4
p1 logout <route>         remove the route's entry from p1's store
```

- `<route>` must be a loaded route file whose credential kind is `api-key`; any other kind is a
  usage error. A LEGACY OAuth route says where its login comes from (`claude` / `codex` CLI). A
  `store_only` OAuth route says instead that its credential is read from p1's own store, that
  `p1 login` cannot write an OAuth entry yet, that p1 has no independent OAuth flow, and that the
  CLI login is NOT read (ADR-0061, §8) — it never pretends a browser flow exists. Unknown route →
  error listing the routes.
- The key is read from STDIN, never from an argument (arguments land in shell history and in
  `ps`). On a TTY the prompt `key for <route> (input hidden): ` is shown and echo is switched
  off for the read and restored by a guard on every path, including cancel; piped stdin is read
  as is (`p1 login <route> < keyfile`). One line; surrounding whitespace trimmed; the same
  format check as every other key source (printable ASCII, no spaces); empty → error, nothing
  written.
- Writing: create `~/.config/p1` 0700 and `auth.json` 0600 if missing; an existing file or
  directory with wider permissions is REFUSED with the chmod message (never silently tightened —
  it may be that way on purpose or by accident, the owner decides). Read-modify-write under
  `lock_exclusive`, atomic rename, every other route's entry preserved byte for byte in meaning
  (unknown fields of other entries survive). The entry is `{"type":"api_key","key":…}`.
- Nothing is verified against the network: login stores, the first request verifies. After
  writing, print `stored for <route> · source now: <chosen source per §4>` — if an environment
  variable still overrides the store, the line says so, because that is the surprise ADR-0040
  warned about.
- `logout` removes only that route's entry; a missing entry is reported, not an error; an empty
  object stays a valid file.
- The key never appears in output, errors, the journal or `Debug`; tests seed a sentinel and
  assert its absence everywhere except the store file.

Must-pass: login on a fresh home creates dir 0700 + file 0600 with exactly one entry; a second
route keeps the first; re-login replaces only that entry; piped and TTY-less paths work in tests
(no real TTY needed: the echo guard is behind a small trait with a recording fake); wide
permissions refuse before anything is read from stdin; non-`api-key` route and unknown route are
usage errors (exit 2); `--list` names the chosen source per route and never a value; after login
`resolve` for that route yields the stored key, and an env var still wins; `logout` removes it
and `resolve` falls through to the borrowed login; concurrent logins for two routes (two tasks,
same file) both land.

## 7. Not decided yet

Browser/OAuth login inside p1 (§8.4), macOS paths and Keychain, an OS keyring, key files outside
the known stores, encrypting the store.

## 8. Self-contained routes — `store_only` (ADR-0061)

Owner order, 2026-09-24: every active p1 model route must be self-contained within p1, and p1
must not silently read Pi, OpenCode, Claude Code or Codex login files at runtime.

### 8.1 The field

`[credential] store_only = true` cuts the chain to the documented environment variable (when the
route names one) and p1's own store. Nothing else is constructed, so no other tool's login file
is opened — absent store entry, unusable store entry and rejected store entry alike. It is a
policy, not a kind: on `api-key` it is the same statement as `borrow = []`, and setting it with a
non-empty `borrow` is a load error (`spec.rs::validate`).

Without the field nothing changes: the legacy chain, the same report and the same guidance, which
the tests pin under "explicit legacy configuration".

### 8.2 The shipped set

Every shipped `routes/*.toml` is store-only: `anthropic-subscription` and
`openai-codex-subscription` (the two OAuth kinds) and `glm-subscription`,
`kimi-coding-subscription`, `opencode-go-subscription`, `opencode-go-1-subscription`,
`opencode-go-2-subscription`, `opencode-go-3-subscription`, `opencode-zen-1`, `opencode-zen-2`,
`opencode-zen-3`, `opencode-zen-free`, `cline-pass-1` and `cline-pass-2` (API keys, `borrow = []`). Each OpenCode account is its
own route with its own variable and its own store entry; `opencode-go-subscription` (the Go-3
account) and `opencode-zen-free` (the Zen-1 account) are compatibility aliases: each reaches the
same account as its numbered route by owner convention, not a code feature. Each alias has its own
store entry (or variable: `OPENCODE_API_KEY`, `OPENCODE_ZEN_API_KEY`) that must hold that account's
key; nothing in p1 keeps the two entries equal — the operator's keys-sync writes both. No shipped route
reads another tool's login at runtime.

### 8.3 Migration

- Nothing in the route FILES changes for a route that opts in beyond the field. To restore the
  old chain for a route, delete the line; the field is the only switch.
- A store-only route's credential must be in p1's store (`$XDG_CONFIG_HOME/p1/auth.json`,
  `~/.config/p1/auth.json`, 0600 in a 0700 directory) or in its documented environment variable.
  `p1 login <route>` writes `api_key` entries; an OAuth entry is written by the acquisition step
  below or imported as `{"type":"oauth","access":…,"refresh":…,"expires":…,"account_id":…}`.
- The operator (XO) moved the API keys into p1's store; every route now resolves from there or
  from its variable. `p1 env show <env>` / `p1 login --list` print ` [p1 store only]` so the
  policy is visible per route.

### 8.4 Remaining work — an independent OAuth grant (limitation)

p1's store refreshes an `oauth` entry it holds, but p1 ships no flow that MINTS one, so the two
OAuth routes need a grant placed in the store by another step. Requirements for that step:

- **Mint p1's own grant, do not copy one.** A refresh token rotates; importing a live refresh
  token from Claude Code or Codex would invalidate it in the source file and break that CLI on its
  next refresh. Copying an active token is explicitly rejected as the long-term mechanism.
- **Use primary sources only.** The authorization endpoint, client id, redirect URI and grant type
  per provider must come from the provider's or the client's own source; none are available in this
  checkout, so nothing was invented. The token endpoints and client ids p1 already refreshes with
  (`claude_code.rs`, `codex.rs`, `store.rs`) are the only provider facts p1 owns today.
- **Human at the end is fine.** A browser/device authorization may need one operator action; the
  flow should then store the resulting `oauth` entry in p1's store (0600, atomic, under the lock)
  and never in another tool's file.
- **Status.** The Codex grant from the retired Pi store was transferred by the XO and passed a live
  p1 request. The Claude independent grant is pending an owner login. No credential value appears
  in this repository.

## 9. A route that sends NO credential — `kind = "none"` (issue #134)

Request from brain1 (2026-09-25): in a Claude Code cloud session an EGRESS PROXY adds the provider
key per host after the request leaves the VM, so the session never sees it. A stored placeholder key
only works if the proxy overrides an existing `Authorization` header, and it may not: p1 needs a
route that sends no credential at all.

### 9.1 The kind

`[credential] kind = "none"` is written EXPLICITLY in the route file; it is never inferred from a
missing key. It has no source: `p1_auth::resolve` returns a source that reads nothing — no
environment variable, no store entry, no other tool's login — and an adapter sends no
authentication header for it. `validate` refuses `env`, a nonempty `borrow` or `store_only = true`
on such a route, because each would name a source the route must not read.

### 9.2 What the adapters and the transports do

- `CredentialSource::proxy_injected() -> bool` (default `false`, `true` for this kind) is the one
  signal. Every adapter that would send `Authorization` (and the Responses account-id header) sends
  NONE when it is true: `p1-provider-openai-chat`, `p1-provider-anthropic` and
  `p1-provider-openai`, on both the SSE path and the WebSocket handshake. The placeholder
  `access()` returns has an EMPTY bearer, and no adapter may send it.
- Neither transport refreshes such a route: there is no credential to rotate and no write-back. A
  401/403 that classifies as `Authentication` (not `InsufficientBalance`/`NotEntitled`, which keep
  their own diagnosis) finishes immediately as an Authentication failure whose message names the
  missing PROXY credential and the status — never a key p1 could hold. That holds for the SSE driver
  AND for a WebSocket upgrade refused 401/403, which never enters its refresh phase; both report
  through `p1_provider_http::proxy_refusal_message`, so the wording cannot drift. A direct
  `refresh(rejected)` still refuses the same way, so a caller that asks anyway gets the refusal and
  never a value.

### 9.3 What is visible

- `p1 login --list` prints the kind as `none (proxy-injected)` (`CredentialKind::label`) and the
  source line as `none (proxy-injected) — the egress proxy injects the credential`
  (`CredentialPolicy::ProxyInjected`). `p1 env show` prints the same line.
- `p1 login <route>` and `p1 logout <route>` are usage errors: p1 stores nothing for this route, so
  there is nothing to write and nothing to remove.
- `p1 usage` reports such a route `Unsupported`: p1 has no credential to present to a usage
  endpoint, and probing one with an empty bearer would be a lie.

### 9.4 Must-pass

a. The table parses `none`, `name()` is `none`, `label()` is `none (proxy-injected)`, and
   `env`/`borrow`/`store_only` beside it are load errors. An unknown kind is still a route-file
   error listing the known ones.
b. A `kind = "none"` route composes and its request carries no `authorization` (nor `x-api-key`,
   nor the Responses account id) for EVERY adapter — asserted from the transport's record, through
   the host's own route loader and catalog factory.
c. Nothing is read: with a store file whose mode makes any read fail, and a directory where each
   CLI's login file belongs, the chain still yields the empty placeholder and a request still
   reaches the transport. The files are byte-identical afterwards.
d. A 401 on such a route is an Authentication failure naming the proxy credential and the status,
   with exactly one request (or, on a WebSocket route, one handshake) and no refresh call.
e. An api-key route with the very same route file and a readable store still sends `Bearer <key>`.
