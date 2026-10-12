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
install nothing. The login-format and decoder crates and the host-side test suites keep a home
under `connectors/` (unit 3 of #924 decides the paths; the suites stay a member of the root
workspace).

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

Status: these directories are the layout of ADR-0217. Unit 2 of #924 brought the connectors back
from phaseone-connectors into `connectors/subscriptions/` (history kept); unit 3 regroups them
into the directories above, moves the root `routes/` into `anthropic/` and `openai/`, and makes
the list of anchored catalog roots explicit (`.` and each vendor). Until then the routes live in
`routes/` at the repository root and in `connectors/subscriptions/`, and p1 reads nothing from
the vendor directories.
