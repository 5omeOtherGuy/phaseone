# The credential-method world

A credential method, or a login another tool keeps, as a component (ADR-0212). The world
`p1:module/credential-method` (`modules/wit/worlds.wit`) exports five pure functions and
imports nothing. The class's allocation is empty (`modules/capabilities.toml`), so the loader
refuses a component that imports even a type-only interface.

The host keeps everything else, natively and the same for every module (`p1-auth`):

- it reads and writes the login file through its checked, pinned directory, under the login's
  lock;
- it sends the refresh through the bounded exchange, over HTTPS, only to an origin the
  manifest's `token_origins` declares;
- it keeps the time;
- it decides what a token is worth: the 5-minute refresh margin, a rejected or expired token,
  a header-safe value.

A module is called only for the route whose `[credential]` names it, and only with that
route's login file or that refresh's response.

## The package

A credential package is an ordinary package (`package.md`) of `kind = "credential-method"`,
with `capabilities = []` and one more manifest field:

| Field | Meaning |
|---|---|
| `token_origins` | the `https://host[:port]` origins its refresh requests go to; `[]` for a login that never refreshes |

`p1 modules install` shows the origins and installs the package only with `--confirm-origins`,
both on the first install and whenever the origins differ from the ones the lock pins. The lock
entry pins them beside the digest (`token_origins = [...]`), and a session refuses a package
whose set declares others.

A route names the module by the name it is installed under:
- `kind = "<name>"` for a method;
- `borrow = ["<name>:<key>"]` for a keyed login.

The name is the lock key, or `<name>` for p1's release package `p1/<name>`.

## The exports

Every structured answer is a JSON text. `p1-auth` reads it with closed field sets and refuses
anything else. A reason (the `err` side of a result) is shown to the owner, so it never names a
value of the file or the response.

### `describe() -> string`

```json
{
  "method": {
    "owner": "the Claude Code CLI (`claude`)",
    "store": "oauth",
    "label": "claude-code-oauth",
    "usage_probe": "claude"
  },
  "login": {
    "title": "Claude Code login",
    "guidance": "run Claude Code login",
    "dir_env": "CLAUDE_CONFIG_DIR",
    "dir_env_subdir": null,
    "home_dir": ".claude",
    "file": ".credentials.json",
    "lock": ".credentials.json.lock",
    "route_dir": true,
    "keyed": false
  }
}
```

- **`method`**: present when a route's `kind` may name the module.
  - `store` is `oauth` (p1's store keeps OAuth tokens it refreshes through this module) or
    `api-key`.
  - `label` and `usage_probe` are optional.
- **`login`**: present when the module reads a login file.
  - Where the file is: `<route login_dir>` when the route names one and `route_dir` allows it;
    else `$<dir_env>[/<dir_env_subdir>]`; else `~/<home_dir>`. `file` and `lock` are plain
    file names, and `lock` defaults to `<file>.lock`.
  - A `keyed` login is a store of keys that API-key routes borrow by key. It describes no
    method.

### `read-login(file: list<u8>, key: option<string>) -> result<string, string>`

```json
{"access": "...", "refresh": "...", "expires_at_ms": 1700000000000, "account_id": "..."}
```

- `refresh`, `expires_at_ms` (milliseconds since the Unix epoch) and `account_id` may be absent.
- `{"access": null}` means a keyed login has no entry for `key`.
- `err` means the file holds no usable login.

### `refresh-request(refresh-token: string, file: option<list<u8>>) -> result<string, string>`

```json
{"url": "https://auth.example/oauth/token", "headers": [["content-type", "application/json"]], "body": "..."}
```

- `file` is the borrowed login the token came from, or none for an entry of p1's store.
- The host refuses the request, before it is sent or anything is staged, unless `url` is
  HTTPS to a declared origin.

### `refresh-response(body: list<u8>, sent-at-ms: u64) -> string`

```json
{"access": {"ok": "..."}, "refresh": "...", "expires_at_ms": {"ok": 1700003600000}, "account_id": null}
```

- Each field is checked on its own, so a rotated `refresh` token is kept even when the rest is
  unusable.
- `expires_at_ms` is `{"ok": null}` for a token with no expiry, which p1's store refuses.
- `{"err": "..."}` names why a field is unusable.
- The host calls this only for a 2xx response.

### `merge-login(file, body, sent-at-ms, now-ms, keep-access) -> result<list<u8>, string>`

- The result is the login file with the response written into it, every other field kept.
- When `keep-access` is false, the host refused the new access token: the module writes the
  rotated refresh token and leaves the old access token due for refresh.
- The host writes the result atomically, only when the file still holds the refresh token it
  spent.
