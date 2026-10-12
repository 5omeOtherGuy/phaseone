# The usage-probe world

A plan's usage endpoint and the decoding of its response, as a component (ADR-0214). The world
`p1:module/usage-probe` (`modules/wit/worlds.wit`, WIT 1.2) exports two pure functions and
imports nothing. The class's allocation is empty (`modules/capabilities.toml`), so the loader
refuses a component that imports even a type-only interface.

The module never receives a credential. The host keeps everything else, natively and the same
for every module (`p1-usage`, `HttpProbe`):

- it resolves the account's credential through `p1-auth`, after the probe origin is approved
  for the account (ADR-0110);
- it builds the request from the module's description and attaches the bearer, and the account
  id when the description names its header;
- it sends it over HTTPS, only to an origin the manifest's `token_origins` declares, with a
  20-second timeout, no redirects and no retries;
- it maps a non-2xx status itself, and hands the module only a 2xx response's body;
- it checks every string of the decoded answer again before it is shown (a label, a reset
  instant, a currency, a window detail), and drops any that carries the credential it sent.

An account names its probe by the name it is installed under (`usage = "<name>"`, ADR-0139); an
account that names none takes the probe its credential method's description names
(`usage_probe`, [`credential.md`](credential.md)). A probe runs only for the credential method
its description names.

## The package

A usage-probe package is an ordinary package (`package.md`) of `kind = "usage-probe"`, with
`capabilities = []` and the manifest field credential methods carry:

| Field | Meaning |
|---|---|
| `token_origins` | the `https://host[:port]` origins its request goes to, which receive the account's credential |

`p1 modules install` shows the origins and installs the package only with `--confirm-origins`,
both on the first install and whenever the origins differ from the ones the lock pins. The lock
entry pins them beside the digest, and a session refuses a package whose set declares others.
The name is the lock key, or `<name>` for the release set's package `<namespace>/<name>` of the
class (the subscription connectors ship `connectors/claude`, `codex`, `opencode-go`, `kimi` and
`glm`).

## The exports

Both structured answers are JSON texts. `p1-usage` reads them with closed field sets and refuses
anything else.

### `describe() -> string`

```json
{
  "url": "https://api.example/usage",
  "method": "api-key",
  "headers": [["x-client", "p1-usage"]],
  "account_id_header": null,
  "user_agent": true,
  "borrowed": true
}
```

- `url`: an https url at one of the manifest's `token_origins`, naming no user.
- `method`: the credential method whose credential the probe takes (`api-key`, or a credential
  module's name such as `claude-code-oauth`). A 401 or 402 on an `api-key` probe is a credential
  failure; on any other method the status is reported as it is.
- `headers`: static, non-secret headers, sent after the credential. A probe may not name
  `authorization`, `cookie`, `user-agent` or a connection header.
- `account_id_header`: the header the credential's account id travels in; a credential without
  one is a credential failure, and nothing is sent.
- `user_agent`: whether p1's own `user-agent` (`p1/<version>`) is sent.
- `borrowed`: whether a credential the method borrows from another tool's login may be sent to
  this endpoint without the account's recorded approval of the origin.

### `decode(body: list<u8>) -> result<string, string>`

```json
{
  "plan": "prolite",
  "windows": [
    {"kind": "session", "used_percent": 13.0, "resets_at": "2026-09-24T00:05:01Z",
     "limit_reached": false, "detail": "87/100 left"},
    {"kind": "other", "label": "month", "used_percent": 20.09, "limit_reached": false}
  ],
  "credits": {"balance": 0.0, "reset_credits": 1},
  "extra_usage": {"enabled": true, "used": 12.4, "limit": 5000.0, "currency": "EUR"}
}
```

- `kind` is `session`, `weekly`, `weekly-scoped` or `other`, which `label` names; any other kind
  is an unreadable answer. `scope` names a scoped window's model.
- Every field but `kind` may be absent. An unknown percentage stays unknown: it is never `0`.
- The decoder passes the vendor's strings through; the host keeps a label only when it is short
  plain text, a reset instant only when it is RFC 3339, a currency only when it is three letters.
- `{"credential_refused": true}` says the vendor refused the credential in a 2xx body; the host
  reports a credential failure and keeps nothing else.
- `err` means the body is not this plan's usage response; the host reports it as an invalid
  usage response and keeps nothing.
