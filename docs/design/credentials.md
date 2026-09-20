# Credentials — crate `p1-auth` (ADR-0040, step 5 of ADR-0039)

Status: spec. Credentials belong to the ROUTE. After the provider split the lookup code still
sits in three places (`p1-provider-anthropic/src/credentials.rs`, `p1-provider-openai/src/credentials.rs`,
`p1-host/src/auth.rs`); this step moves it into one crate and adds the precedence chain and the
"which source" report. Borrowing stays the default; §6 adds `p1 login` for pasted API keys (ADR-0044).

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

## 4. Which source — visible, never the value

`CredentialSource` gains nothing. `p1_auth::describe(route_id, &spec, &Locations) -> SourceReport`
is a separate, non-secret probe: `{ chosen: Option<SourceName>, tried: Vec<(SourceName, Presence)> }`
with `SourceName` rendering as `env OPENCODE_API_KEY`, `p1 store`, `opencode login`, `pi login`,
`Claude Code login`, `Codex login`, and `Presence` = `present | absent | unusable(<reason>)`.
`p1 env show` prints one line `credential  <chosen>` (or `credential  none — <what to do>`), and
the TUI's `/status` can use the same report. The report reads files to see whether an entry
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

## 6. `p1 login` — pasted keys into p1's own store (ADR-0044)

Owner, 2026-09-20: "Keep the store. You can dispatch a worker to work on the login
implementation." Scope: API KEYS. Browser/OAuth logins stay borrowed from the official tools.

```
p1 login <route>          read one key, store it for that route
p1 login --list           every route: its credential kind and the source report of §4
p1 logout <route>         remove the route's entry from p1's store
```

- `<route>` must be a loaded route file whose credential kind is `api-key`; any other kind is a
  usage error that says where that login comes from (`claude` / `codex` CLI). Unknown route →
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

Browser/OAuth login inside p1, macOS paths and Keychain, an OS keyring, key files outside the
known stores, encrypting the store.
