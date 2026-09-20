# How pi and opencode store login credentials — and what p1 should do

Read-only research, 2026-09-20. **No credential value was read.** JSON files were inspected
with `jq 'paths(scalars)|map(tostring)|join(".")'` and a type-substituting `walk`, or with
`jq 'to_entries[]|"\(.key)\ttype=\(.value.type)"'`; only key names and `type` literals were
printed. File/dir modes came from `stat -c '%A %a %n'`, counts/schemas from `sqlite3
.schema`/`pragma table_info`/`select count(*)`. Every claim below is cited to a source file +
function or to the exact observation command. Anything not verified is marked **UNVERIFIED**.

Basis of truth: pi **0.85.1** installed at
`~/.local/share/pi-node/node-v22.23.2-linux-x64/lib/node_modules/@earendil-works/pi-coding-agent`
(dist JS + bundled `docs/`); opencode **1.18.31** (`opencode --version`), whose source was read
from the public repo at tag `v1.18.31` (`sst/opencode` → `anomalyco/opencode`); dev-branch
`packages/opencode/src/auth/index.ts` is byte-identical to `v1.18.31` (`diff -q`).

## 1. pi

| | |
|---|---|
| Location | `join(getAgentDir(), "auth.json")` — `dist/core/auth-storage.js:AuthStorage.create`, `dist/config.js:getAuthPath`. `getAgentDir()` = `$PI_CODING_AGENT_DIR` else `~/.pi/agent` (`dist/config.js:ENV_AGENT_DIR`, `getAgentDir`). No XDG use; not per-OS branched (**UNVERIFIED** for Windows). |
| Observed | `~/.pi/agent/auth.json` mode **600**; dirs `~/.pi`, `~/.pi/agent` mode **775** (`stat`). |
| Format | One JSON object, keys = provider ids (not routes). Discriminated by `type`: `api_key {key, env?}` and `oauth {access, refresh, expires (number, ms epoch), accountId?}` — validated in `dist/core/auth-storage.js:ReadOnlyAuthStorage.load`; `oauth` shape written by `dist/cli/...`/bundle `openai-codex.js:credentialsFromToken`. `key` may be `$ENV`, `"!command"`, or a literal (`docs/providers.md` "Key Resolution"). |
| Observed schema | keys `opencode-go/kimi-coding/openrouter/zai` → `type=api_key`, field `key`; `openai-codex` → `type=oauth`, fields `access,refresh,expires,accountId` (`jq 'paths(scalars)'`). |
| Protection | `writeFileSync(..., {mode:0o600})` — mode applied only on create (`AUTH_FILE_WRITE_OPTIONS`); parent `mkdirSync(mode 0700)` **only if it doesn't already exist** (here 775). All reads and writes go through `proper-lockfile` on the auth path: `FileAuthStorageBackend.withLockSync/withLockAsync` (10 sync attempts, async retries to a 30 s deadline, `stale:30_000`). Write is **in place, not temp+rename** → a crash mid-write can truncate. **No keyring/secret-service** in the code read. |
| Lifecycle | `/login` (interactive: browser callback or device-code flow, or paste key) and `/logout` (`docs/providers.md:26,62`); CLI `pi auth print-api-key\|print-bearer-token\|check` (`dist/cli/auth-command.js`, `credential-print.js`) deliberately emits a token on stdout. Refresh: provider supplies `refresh(credential, signal)`; runtime calls `credentials.modify(providerId, current => …)` so re-read/check/refresh/write are all inside the file lock, and only refreshes if `current` is still near expiry (`chunk-IDDQWTHI.js:resolveStoredOAuth`, `DEFAULT_OAUTH_MINIMUM_VALIDITY_MS = 300_000`, 15 s refresh timeout). |
| Precedence | `docs/providers.md:139`: "Auth file credentials take priority over environment variables." Per-entry `env` map is consulted before the process env; a runtime API-key overlay (`dist/core/runtime-credentials.js:RuntimeCredentials`) sits above the store. |
| Multi-account | None — one credential per provider id; extra accounts only by declaring extra custom providers in `models.json` (e.g. Radius OAuth). |
| Sharp edges | in-place write (not crash-atomic); one malformed/unknown-`type` entry makes the **whole file fail to load** (`load()` throws); `auth.json.lock` (a proper-lockfile directory) can be left stale after `kill -9`; `pi auth print-*` can leak a token into shell history/logs; containing dir is 775, so entry names are world-visible. |

## 2. opencode

| | |
|---|---|
| Location | `path.join(Global.Path.data, "auth.json")` — `packages/opencode/src/auth/index.ts` (`const file = …`); `Global.Path.data = path.join(xdgData!, "opencode")` (`packages/core/src/global.ts`), i.e. `$XDG_DATA_HOME/opencode/auth.json` else `~/.local/share/opencode/auth.json` (xdg-basedir). Config/state/cache go to the other XDG dirs; `OPENCODE_CONFIG_DIR` redirects **config only**. Env overrides: `OPENCODE_TEST_HOME` (home), `OPENCODE_AUTH_CONTENT` (replaces the whole file content), `OPENCODE_DB`. |
| Observed | `~/.local/share/opencode/auth.json` mode **600**, dir **775**; a sibling `auth.json.bak-20260919-105303` also 600 (`stat`). Also `~/.config/opencode/` (config, no creds). |
| Format | JSON object, provider-id keyed, tagged union on `type`: `api {key, metadata?}` \| `oauth {refresh, access, expires (ms epoch), accountId?, enterpriseUrl?}` \| `wellknown {key, token}` (`auth/index.ts:Oauth/Api/WellKnown`, identical at dev and v1.18.31). |
| Observed schema | 4 entries `type=api` with field `key`; 1 entry (`zai`) `type=api_key` (`jq 'to_entries[]'`). `api_key` is **not** in the 1.18.31 schema (`Literal("api")`), and `all()` drops values that fail `Schema.decodeUnknownOption` (`Record.filterMap`), so that entry is silently ignored (and lost on the next `Auth.set`). Confirmed only by reading the code — **UNVERIFIED** at runtime (opencode was not run beyond `--version`). |
| Protection | No lock at all. `Auth.set`/`remove` do read-all → `{...data, [id]: info}` → `FSUtil.writeJson(file, data, 0o600)`, and `writeJson` is `fs.writeFileString(path, JSON.stringify(...))` **then** `fs.chmod(path, mode)` (`packages/core/src/fs-util.ts:writeJson`) — not atomic, mode applied after the bytes land, so a new file is briefly at umask perms. No keyring. A v2 DB-backed store exists in 1.18.31 (`packages/core/src/credential.ts`, `Credential` service over the `credential` table) but is empty here (`select count(*) from credential` → 0); the `account` table (`access_token/refresh_token/token_expiry`) is also empty, and `opencode.db` is mode **644**. |
| Lifecycle | Login through the TUI/server provider-auth flow: `ProviderAuth.authorize` returns `{url, method:auto\|code, instructions}`, `ProviderAuth.callback` then calls `auth.set(providerID, {type:"oauth"|"api", …})` (`packages/opencode/src/provider/auth.ts`). Provider hooks implement browser callback (Codex: `127.0.0.1:1455/auth/callback`) or device code (`plugin/openai/codex.ts`). Refresh happens inside each plugin's custom `fetch`: if `expires < Date.now()` it calls `refreshAccessToken` then `input.client.auth.set(...)` (`plugin/openai/codex.ts` ~341–400). In-process single-flight via a `refreshPromise`; **no cross-process coordination**. Removal: `Auth.remove`. |
| Precedence | Env var first, then `auth.json` (`api` key, then `oauth` access), then config provider options — e.g. `provider.ts` `snowflake-cortex` (`envToken ?? apiKeyToken ?? oauthToken ?? configToken`) and the custom `opencode` provider's `hasKey` check. `OPENCODE_AUTH_CONTENT` overrides everything, from the environment. |
| Multi-account | v1 auth.json: one per provider id. The v2 tables hint at more: `credential` has `label` + `active` with a unique index per integration, and `account` is keyed by email (`pragma table_info`, `packages/schema/src/credential.ts`) — in flux. |
| Sharp edges | no lock + non-atomic write (two processes refreshing the same provider clobber each other's rotated token); schema-mismatched entries silently dropped and then destroyed on the next write; credentials can be injected via an env var; the incoming DB store lives in a **0644** file; data dir (not config) means credentials aren't covered by a config-only backup. `packages/core/src/util/flock.ts` exists but I found no use of it on `auth.json` (**UNVERIFIED** whether any non-auth path uses it). |

## 3. p1's current stance (for continuity)

p1 reuses Claude Code's `$CLAUDE_CONFIG_DIR/.credentials.json` and Codex's `$CODEX_HOME/auth.json`,
re-reads on every `access`, and on refresh takes `p1-provider-http::lock_exclusive` on a sibling
`.lock`, re-reads, then writes atomically (temp + `rename`, 0600 from creation), preserving unknown
fields (`crates/p1-provider-anthropic/src/credentials.rs`, `crates/p1-provider-openai/src/credentials.rs`,
`crates/p1-provider-http/src/file_lock.rs`; `docs/adr/0020-…`, `docs/design/routes.md` §Auth).
That is already the good half of pi's design (re-read under lock) plus the crash-safety pi lacks.

## 4. Recommendation

Requirements: Linux-first, single user, lean; keep reusing Claude Code / Codex / opencode / pi
logins; add p1's own store for routes with none; no credential in logs, journal,
`ResolvedEnvironment`, or the shell tool's env.

| Option | What it buys | Cost / risk |
|---|---|---|
| **A. Own JSON file at `$XDG_CONFIG_HOME/p1/auth.json`** (fallback `~/.config/p1/auth.json`), one module `p1-auth`, provider-id keyed, `type: api_key\|oauth` with pi's field names (`access`, `refresh`, `expires` ms, `account_id`) | Zero new deps: reuses the existing `lock_exclusive` + temp/rename 0600 writer; trivially interoperable with pi/opencode schemas (import/export one JSON object); `~/.config` is the conventional owner-config location and is what a user backs up | Plaintext at rest, like both CLIs; must be disciplined about never `Debug`-ing or `Serialize`-ing it |
| **B. Same, under `$XDG_DATA_HOME/p1/auth.json`** | Same mechanics; matches opencode's choice; data dir can be on a separate volume | Credentials mixed into runtime data; not what people expect from "config"; no real gain over A |
| **C. OS keyring (secret-service) with file fallback**, e.g. the `keyring` crate | Encrypts at rest, protects against backup theft / other local users | Real cost: crate + D-Bus/zbus stack, a running secret service, headless/CI failure paths, tests need a fake — and the fallback file must exist anyway, so the plaintext path is never retired. On a single-user box any same-uid process that can read the file can also use the unlocked keyring, so the practical gain against the threat that matters (a compromised agent/process) is small |
| **D. Reuse-only, no store** | Smallest surface | Fails the brief: a route with no co-installed CLI can never be logged in |

**Pick A.** Put p1's own store in `$XDG_CONFIG_HOME/p1/auth.json` (0600, `~/.config/p1` 0700
when p1 creates it), keyed by provider/route id, with a small `p1-auth` module exposing
`read(id)` and `modify(id, f)` implemented with the existing `lock_exclusive` on a sibling
`auth.json.lock` and the existing atomic 0600 writer. Resolution order per route:
explicit env override (documented, opt-in) → p1 store → reuse the other CLI's file → error
naming the login command. Keep `Credential`'s `<redacted>` `Debug` and never add credentials
to `ResolvedEnvironment`, the journal, or the shell tool env. Keep the keyring (option C) as a
documented, feature-gated future add-on, not the default.

## Not verified / open

- Did **not** run opencode beyond `--version`; the `zai api_key`-entry drop is inferred from
  `Record.filterMap(decode(...))`, not observed.
- pi's per-OS path handling (Windows/macOS) not inspected; config is plain `homedir()`-based.
- Neither project showed any keychain/keyring use, but I did not exhaustively audit every file
  (opencode's provider plugins and pi's bundled chunks were grep-searched, not fully read).
- opencode's `OPENCODE_DB`/v2 `credential`+`account` tables are not yet populated here, so their
  live behavior (multi-account, locking) is **UNVERIFIED**.
- p1's Codex credential-field layout itself is still `[todo-live]` in `docs/design/routes.md`.
