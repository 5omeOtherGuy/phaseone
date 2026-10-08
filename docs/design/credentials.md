# Credentials — crate `p1-auth` (ADR-0040, step 5 of ADR-0039; self-contained routes, ADR-0061)

Status: spec. Credentials belong to the ROUTE. After the provider split the lookup code still
sits in three places (`p1-provider-anthropic/src/credentials.rs`, `p1-provider-openai/src/credentials.rs`,
`p1-host/src/auth.rs`); this step moves it into one crate and adds the precedence chain and the
"which source" report. Borrowing stays the default; §6 adds `p1 login` for pasted API keys (ADR-0044).
§8 adds the opt-in `store_only` policy (ADR-0061): every SHIPPED route is now self-contained and
reads no other tool's login at runtime, and minting an independent OAuth grant is the remaining work.
§10 adds `login_dir` and `p1 login <route> --from-claude-code` (ADR-0074): a `claude-code-oauth`
route may borrow a NAMED Claude Code directory, and a Claude Code login can be imported into p1's store.

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
| `claude-code-oauth` | `env` optional, `login_dir` optional (§10) | env var (a bearer token, no refresh) → p1 store → Claude Code's login file (`<login_dir>/.credentials.json`, else `$CLAUDE_CONFIG_DIR/.credentials.json`, else `~/.claude/.credentials.json`) |
| `codex-oauth` | `env` optional | env var → p1 store → Codex CLI's login file |
| `none` | none | NOTHING. An egress proxy injects the provider's credential after the request leaves the process, so no variable, store entry or login is read, and the adapter sends no authentication header (§9) |

`store_only` (boolean, default `false`) is the one policy field on the table, for every kind
(ADR-0061, §8). Written, the chain stops after p1's own store: the documented variable and the
store entry, and no other tool's login. Absent, the rows above are unchanged.

`kind = "none"` is not a policy but a KIND with no source at all (issue #134, §9). Naming a
source beside it is a contradiction, so `env`, a nonempty `borrow` or `store_only = true` on a
`none` route is a load error.

`login_dir` (ADR-0074, §10) is allowed ONLY on `claude-code-oauth`: the same field on any other
kind is a load error, and so is a directory that is neither absolute nor below the home
(`~/<dir>`; the home itself, `~` or `~/`, is refused), one with a `.` or `..` component, and
`/` itself. An absolute `login_dir` that is the home names no login directory.

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
- **An OAuth token refresh is bounded** (issue #164, extending ADR-0069's read bounds to the
  refresh call): the response headers must arrive within `FIRST_BYTE_TIMEOUT` (120 s), and each
  read of the JSON body within `STREAM_IDLE_TIMEOUT` (300 s) — the same constants as provider
  reads, from `p1-provider-http`, applied by one helper (`p1-auth/src/refresh_http.rs`) for the
  Claude Code login, the Codex login and p1's store `oauth` entries. Expiry is an
  `Authentication` error `token refresh got no response within <n> s` — never a body, token or
  header; nothing is written, so the file stays byte-identical, and the lock is released. The
  whole exchange also ends by `REFRESH_DEADLINE` (300 s) however slowly the body trickles, its
  body is capped at 64 KiB, and a non-success status is reported without reading its body
  (issue #484). A peer waits up to `LOCK_PATIENCE` (360 s), longer than the longest bounded
  refresh (`REFRESH_DEADLINE`, asserted at compile time), so a peer queued behind a holder
  that times out gets its turn and refreshes itself.
- **Credential files are handled through a checked, pinned directory** (issue #484,
  `p1-auth/src/credential_file.rs`). Before a store or login file is read or written, its
  directory and every directory above it (as spelled and as resolved) must be owned by the
  user or root and writable by nobody else (sticky directories and the user's own primary
  group excepted); p1's store directory must also be 0700. The directory is opened once and
  every later step uses that handle. A file is opened without following a symlink, must be a
  regular file owned by the user (in p1's store also 0600 with one link) and at most 1 MiB. The
  lock is the directory itself plus the legacy sibling `.lock` file. A replacement is staged in
  a new `O_EXCL` 0600 file `.<name>.p1-<pid>-<nanos>-<n>.tmp`, reserved BEFORE the refresh
  request, synced, re-checked, renamed and the directory synced.
- **A rotation the server performed is never lost** (issue #484). Once the refresh request is
  sent, the rotation runs to its write-back in its own task even when the caller is
  cancelled. A response whose access token is unusable (missing, not header-safe, a zero
  lifetime, or the rejected token) still keeps a rotated refresh token, with the access token
  marked expired, and then fails. A publication that fails keeps the rotated file as
  `.<name>.p1-unsaved`, which the next refresh adopts under the lock unless the file changed
  since. A login another writer replaced while the request was out is never overwritten.
  Lifetimes count from when the request was sent.
- **What a source accepts** (issue #484): access tokens and account ids must be header-safe
  (printable ASCII, no spaces); an expiry must be a millisecond timestamp (absent or `null`
  means none); a Codex JWT whose `exp` is not a timestamp is refused; a changed variable is
  held to the same rule on refresh; and a refresh rotates only the source that issued the
  rejected credential.
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

**Where a used credential can show up (issue #484, ADR-0108).** The host registers every
credential a route's source hands out in its `p1_redact::SecretSet` before an adapter sees it.
Tool output masks registered values and credential shapes; everything a provider streams (text,
reasoning, tool input, the committed item, failure messages) masks registered values, holding back
a possible value prefix between deltas; replay data is carried verbatim. The line front end's
`ToolStarted` and authorization-ask input summaries additionally mask credential shapes before
flattening line breaks and truncating the display; this does not rewrite the dispatched input
(issue #160). Shape masking recognizes `sk-` at a word boundary (machine-consumed declaration
checks still detect glued keys), complete JSON auth-field names rather than suffixes such as
`monkey` or `public_key`, and whitespace with one line break after `Bearer` or `Authorization:`.
The marker's byte count excludes the retained `sk-` family prefix. A route that takes the id
of a shipped route may only send its credential to the shipped endpoint origin
(`routes::check_shipped_origin`); `kind = "none"` sends none and is exempt. ADR-0110
extends this to credential sources and new ids (§11); the shipped trust anchor is compiled,
not read from the installation prefix.

**File-service refusal (issue #160).** The host composes one explicit path list from
`Locations::credential_paths()` using its injected home/environment, then adds every loaded
route's resolved `login_dir/.credentials.json`, including routes not currently selected.
The list includes all resolved stores/logins and the config keys directory; default-home
refusals remain in force. File services and `Workspace` share these inputs, with no
process-environment lookup in the module runtime. Each request and mutation stage resolves
the path spellings afresh, preserving symlink-retarget checks and opened-descriptor identity
checks. Innocently named hard links to route logins and credential-file family members are
refused on read, stat, search and mutation, just like their protected names.

## 5. Must-pass

a. Precedence for each kind: env beats store beats borrowed; absent sources are skipped; an
   unusable earlier source is an error, not a skip.
b. Rotation: a changed file value / a newly set variable is seen by the next `access()`.
c. Write-back: a refreshed borrowed OAuth token lands in the file it came from; a refreshed p1
   store entry lands in p1's store; nothing else in either file changes BY VALUE — every other
   key, inside the OAuth object and in other entries, compares equal as JSON (p1 writes its own
   pretty layout, so the bytes of a compact file change; issue #484 made this the stated
   contract); the two existing refresh-contention tests move with the code and pass unchanged,
   and a two-process test holds the lock across processes.
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
i. Every shipped `routes/*.toml` sets `store_only = true` and lists no nonempty `borrow` — except
   `anthropic-subscription-2`, which borrows its account's Claude Code login in its `login_dir`
   by owner order (ADR-0074, §10).

## 6. `p1 login` — pasted keys into p1's own store (ADR-0044)

Owner, 2026-09-20: "Keep the store. You can dispatch a worker to work on the login
implementation." Scope: API KEYS. Browser/OAuth logins stay borrowed from the official tools.

```
p1 login <route>          read one key, store it for that route
p1 login <route> --from-claude-code [DIR]
                          copy the Claude Code login in DIR into p1's store (§10, ADR-0074)
p1 login <route> --trust-endpoint
                          approve the endpoint origin for an environment-keyed API route (§11)
p1 login --list           every route: its credential kind and the source report of §4
p1 logout <route>         remove the route's entry from p1's store
```

- `<route>` must be a loaded route file whose credential kind is `api-key`; any other kind is a
  usage error. A LEGACY OAuth route says where its login comes from (`claude` / `codex` CLI). A
  `store_only` OAuth route says instead that its credential is read from p1's own store, that
  `p1 login` reads no OAuth grant from stdin, that p1 has no independent OAuth flow, and that the
  CLI login is NOT read (ADR-0061, §8) — it never pretends a browser flow exists. A
  `claude-code-oauth` route's message also names `p1 login <route> --from-claude-code [DIR]`
  (§10). Unknown route → error listing the routes.
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
- Login also records the endpoint origin in protected `auth.json.origins` metadata (§11).
  Claude Code import records it too. `--trust-endpoint` records only the origin, reading no
  stdin, key variable or credential document and storing no key. It accepts `api-key`
  routes and `store_only` Claude Code/Codex OAuth routes; borrowed OAuth and `none`
  routes are refused. It neither imports an OAuth grant nor verifies one exists.
- Nothing is verified against the network: login stores, the first request verifies. After
  writing, print `stored for <route> · source now: <chosen source per §4>` — if an environment
  variable still overrides the store, the line says so, because that is the surprise ADR-0040
  warned about.
- `logout` removes only that route's entry; a missing entry is reported, not an error; an empty
  object stays a valid file. Since ADR-0074 `logout` also removes a `claude-code-oauth` or
  `codex-oauth` route's store entry (an imported copy, §10); for those routes a missing entry is a
  usage error, because their login lives with the CLI and `logout` cannot touch it.
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

Every shipped `routes/*.toml` is store-only except ONE, `anthropic-subscription-2` (the second
Claude subscription, ADR-0074, §10.3), which borrows its account's Claude Code login in its
`login_dir` by owner order. The store-only set: `anthropic-subscription` and
`openai-codex-subscription` (the two OAuth kinds) and `glm-subscription`,
`kimi-coding-subscription`, `opencode-go-subscription`, `opencode-go-1-subscription`,
`opencode-go-2-subscription`, `opencode-go-3-subscription`, `opencode-zen-1`, `opencode-zen-2`,
`opencode-zen-3`, `opencode-zen-free`, `cline-pass-1` and `cline-pass-2` (API keys, `borrow = []`). Each OpenCode account is its
own route with its own variable and its own store entry; `opencode-go-subscription` (the Go-3
account) and `opencode-zen-free` (the Zen-1 account) are compatibility aliases: each reaches the
same account as its numbered route by owner convention, not a code feature. Each alias has its own
store entry (or variable: `OPENCODE_API_KEY`, `OPENCODE_ZEN_API_KEY`) that must hold that account's
key; nothing in p1 keeps the two entries equal — the operator's keys-sync writes both. Apart from
`anthropic-subscription-2`, no shipped route reads another tool's login at runtime.

### 8.3 Migration

- Nothing in the route FILES changes for a route that opts in beyond the field. To restore the
  old chain for a route, delete the line; the field is the only switch.
- A store-only route's credential must be in p1's store (`$XDG_CONFIG_HOME/p1/auth.json`,
  `~/.config/p1/auth.json`, 0600 in a 0700 directory) or in its documented environment variable.
  `p1 login <route>` writes `api_key` entries; an OAuth entry is written by the acquisition step
  below or imported as `{"type":"oauth","access":…,"refresh":…,"expires":…,"account_id":…}` —
  for a `claude-code-oauth` route by `p1 login <route> --from-claude-code [DIR]` (§10).
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
- **Since ADR-0074** a Claude Code login CAN be copied into p1's store
  (`p1 login <route> --from-claude-code`, §10.2), with the rotation risk above stated there, and
  the second Claude route borrows its login in place instead of holding a grant (§10.3).

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

## 10. A named Claude Code login and its import (ADR-0074, issue #199)

Owner order, 2026-09-25: "implement a second claude code subscription provider, so we can switch
between them when our quota is reached". A second subscription is a second Claude Code login,
kept in its own config directory (`CLAUDE_CONFIG_DIR=~/.claude-2 claude`).

### 10.1 `login_dir`

`[credential] login_dir = "<dir>"` names the Claude Code config directory a `claude-code-oauth`
route borrows: `<dir>/.credentials.json` replaces the default file in the chain of §2. A
`~/<dir>` value is expanded against the home directory of the `Locations` at use time; any other
value must be absolute, and the home itself (`~`, `~/`) is refused at load. Absent keeps the default directory (`$CLAUDE_CONFIG_DIR`, else
`~/.claude`); a route that names one never falls back to the default. The named login is handled
exactly like the default one — read fresh on every `access`, refreshed under its own `.lock`,
re-read under the lock, written back to the same file — and nothing is copied. `store_only`
still means the borrowed login is never opened; `login_dir` then only names the default
directory of the import below. Any other kind with `login_dir` is a load error (§2).

### 10.2 `p1 login <route> --from-claude-code [DIR]`

- `<route>` must be a loaded `claude-code-oauth` route; any other kind (`api-key`, `codex-oauth`,
  `none`) is a usage error (exit 2) naming the kind. `DIR` defaults to the route's `login_dir`,
  else the default directory; a leading `~` is expanded.
- `DIR/.credentials.json` is read (either Claude Code shape, as the borrowed source reads it)
  and written into p1's store under the route id as exactly
  `{"type":"oauth","access":…,"refresh":…,"expires":…,"account_id":…}`, with `null` for a field
  the login does not record. `account_id` is `oauthAccount.accountUuid` from `DIR/.claude.json`
  when that file has one.
- A directory with no `.credentials.json` is a usage error naming the file and how to log in
  there (`CLAUDE_CONFIG_DIR=<DIR> claude`, then `/login`); a malformed login is an error naming
  the file. Nothing is written in either case.
- The write is the store's own (`p1_auth::store::import_claude_code_login`): lock, read-modify-
  write, atomic 0600 file, 0700 directory when created, a wider existing file or directory
  refused with the chmod message, every other entry preserved.
- The output is `imported the Claude Code login in <DIR> for <route> · source now: <§4 line>`,
  then a line saying that this p1 store entry wins over any Claude Code login the route borrows
  until `p1 logout <route>` removes it. No token is ever an argument or appears in output, an
  error or a log.
- **An imported copy wins over the live login until `p1 logout <route>`.** p1's store precedes
  the borrowed login in the chain (§2), so on a route that also borrows (`anthropic-subscription-2`)
  the copy shadows the live `~/.claude-2` login — also after Claude Code rotated it. `p1 logout
  <route>` removes the entry (every other entry left as it was), and the route borrows the live
  login again. Importing again replaces the entry; there is never a second one.
- An imported refresh token is a COPY (the rotation risk of §8.4): the first refresh on either
  side invalidates the other. The import is for a machine where only p1 uses the login (an EC2
  box: the route then reads p1's store), or to be repeated after Claude Code rotated it.

### 10.3 The second shipped route

`routes/anthropic-subscription-2.toml` (environment `claude2`) is `anthropic-subscription` with
its own id and origin and `kind = "claude-code-oauth"`, `login_dir = "~/.claude-2"`, without
`store_only`: p1's store entry for the route wins when there is one, else the second account's
Claude Code login is borrowed in place. `p1 usage` labels it `claude max 2` and probes it with
its own credential.

### 10.4 Must-pass

`login_dir` parsed, expanded and used (a scratch directory with a fake `.credentials.json`;
absent → the default directory); `login_dir` on another kind is a load error; the import writes
exactly the store's `oauth` shape, 0600, refuses a non-`claude-code-oauth` route and a missing
login as usage errors, and no output or error carries a token; an import twice replaces the
entry; `p1 logout` removes an OAuth entry with every other entry byte-identical and is a usage
error for an OAuth route without one; a refresh through a `login_dir` writes back to
`<login_dir>/.credentials.json` (0600, atomic) and never touches `~/.claude`
(`crates/p1-auth/tests/login_dir.rs`, `crates/p1-host/tests/login.rs`).

## 11. Credential sources bound to endpoint origins (ADR-0110, issue #486)

The host compiles the source tree's `routes/*.toml` table of id, endpoint origin and credential
kind into the binary. A malformed shipped TOML file fails the build. Missing, changed or
malformed installation-prefix route files cannot remove or replace this anchor.

Before every credential access or refresh, the host checks (construction remains lazy).
Inspection (`p1 env show`, `p1 models`, `p1 login --list`) checks before calling `describe`:
unapproved routes report the origin as not approved and name the approving login command,
without looking up key variables or opening credential documents. Approved routes retain
source-presence reporting. The runtime checks are:

- A shipped id keeps its compiled origin (`check_shipped_origin`), except `kind = "none"`.
- A borrowed OAuth kind (without `store_only`) or any `borrow` list may reach only the
  compiled origins of shipped routes with that credential kind, whatever the new route id.
- A new API-key id (environment or store) or a custom store-only OAuth id requires an origin
  approved for that id in p1's own credential store. Missing/mismatched approval refuses
  before credential resolution, naming `p1 login <id>` and `p1 login <id> --trust-endpoint`.
  API routes that also borrow must satisfy both checks.
- Loopback (`127.0.0.1`, `::1`, `localhost`, optional port) needs no approval record. This
  supports tests and local proxies, not lookalike hosts or userinfo tricks. A shipped id
  remains subject to its shipped-origin rule. A `none` route needs no approval.

Origins are `scheme://authority`, case-folded, ignoring endpoint paths. An explicit port,
changed scheme or userinfo changes the origin; matching fails closed.

Approvals are an object of route ids to origin strings in `auth.json.origins`, beside p1's
credential document. The existing credential-file family policy protects this file and its
staging/recovery siblings from model reads and writes, including XDG overrides. It uses the
same pinned private directory, 0600 staged writer and store lock. Refusal reads only origin
metadata, never `auth.json` or a borrowed credential file. Login revokes old approval under
the lock before replacing the credential, then publishes its origin approval; interrupted
writes leave the new key untrusted. `--trust-endpoint` changes only metadata, preserving any
existing key or OAuth grant. Logout removes both the key/import and approval, or an approval alone.

Origin-bound store presence, access and refresh check metadata under the login writer's lock
before opening the credential document. This covers fresh replacement tokens found after
waiting for a lock as well as refresh results. A recorded origin must match the destination,
even on shipped/borrowed-kind routes; an absent record remains permitted only where the
compiled-origin policy exempts legacy entries. The host rechecks approval after acquisition.
`resolve_with_store_origin` supplies the destination and whether a record is mandatory;
low-level `resolve` remains available to callers without route/destination metadata.

Usage probes check their actual URL origin, not the route's chat URL, before resolving any
credential. Borrowed kinds require a same-kind shipped origin; API-key and store-only ids
require recorded approval for that probe origin. A mismatch produces a skipped probe with a
reason and reads no key variable or credential document. A different usage host (Kimi's
`.com` versus the shipped `.ai` chat host) is not implicitly approved. Store acquisition
uses the same locked origin check, and approval is rechecked before sending the probe.

Existing shipped routes require no approval migration. Custom remote API routes require
`p1 login <id>` for stored keys, or `p1 login <id> --trust-endpoint` for environment keys.
Store-only OAuth routes can use `--trust-endpoint` to approve their endpoint without
replacing the stored grant, including approval needed by a same-origin usage probe.
A borrowed source cannot be redirected to an arbitrary remote proxy by approving its origin;
use a store-only route with an explicitly approved stored/environment credential instead.
