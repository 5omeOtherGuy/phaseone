# Connectors

One connector per vendor: the wire that reaches that vendor, and nothing about how an agent
works (ADR-0217). A connector is laid out as a pack root (ADR-0201) and is a catalog root of the
build and of the share:

- `routes/`: its routes, API key and login alike; one route per wire and credential mode (the
  Anthropic key route and the Claude Code login route are two routes that never fall back to
  each other);
- `profiles/`: only a variant that exists for this vendor alone (`glm-5.3-flash-clinepass`);
  the canonical first-party profiles stay in `profiles/` at the repository root;
- `components/`: the sources of its login (credential-method) and usage-probe modules, built in
  the `connectors/` guest workspace with the module SDK and discovered as
  `connectors/*/components/*/`, packages in the `connectors/` namespace (ADR-0212, ADR-0214),
  shipped in the release's module set;
- `README.md`: its routes and modules, what each login borrows, and which route it prefers for
  a model several of its routes bind.

A connector installed by a user as a pack carries, beside its routes, an installed module set
`modules/` (`manifest.json` and `packages/`, ADR-0209) and a `modules.lock`; the sources alone
install nothing.

Beside the vendors, two directories that are not connectors:

- `shared/`: the crates the components compile in, `shared/logins` (`p1-connector-logins`, the
  Claude Code, Codex, OpenCode and pi login formats as the pure functions of the
  `credential-method` world, ADR-0212) and `shared/usage` (`p1-connector-usage`, the usage
  endpoints and decoders of the `usage-probe` world, ADR-0214);
- `tests/`: `p1-connector-tests`, the host-side suites, a member of the root workspace: they
  drive the logins through `p1-auth`'s credential chain and the probes through `p1-usage`'s
  ledger natively, and compose every connector's routes through the host
  (`cargo test -p p1-connector-tests`).

The guest crates are a workspace of their own (`Cargo.toml` here), which names phaseone only
through `p1-module-sdk`, pinned there once. `scripts/build-modules.sh --all` builds the packages
beside p1's own. `catalog-roots.txt` at the repository root lists `.` and every connector: the
one list the credential anchor (`crates/p1-host/build.rs`), a debug build's catalog,
`scripts/stage-release.sh`, the gate and `release.yml` read (ADR-0217 Decision 4). A new
connector is a directory here and a line there.

The owner's "ChatGPT bundle" is `openai/` plus the `gpt` kit under `kits/`. A kit never lives
in a connector: a family reaches p1 through several vendors.

| Connector | Routes | Login module | Usage probe |
|---|---|---|---|
| `anthropic` | `anthropic` (API key), `anthropic-subscription` (Claude Code login) | `claude-code-oauth` | `claude` |
| `openai` | `openai`, `openai-chat` (API key, SSE), `openai-codex-subscription` (Codex login, WebSocket with SSE fallback) | `codex-oauth` | `codex` |
| `zai` | `glm-subscription`, `glm-messages` | | `glm` |
| `opencode` | `opencode-zen`, `opencode-go-subscription`, `opencode-go-messages`, `opencode-go-glm` | `opencode` | `opencode-go` |
| `kimi` | `kimi-coding-subscription` | | `kimi` |
| `cline` | `cline-pass` | | |
| `pi` | none: the exception, a credential source, a login borrowed from the pi CLI | `pi` | |

Status: unit 2 of #924 brought the connectors back from phaseone-connectors (history kept),
and unit 3 regrouped them into the directories above and moved the root `routes/` into
`anthropic/` and `openai/`.
