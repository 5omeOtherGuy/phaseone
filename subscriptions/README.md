# Subscription connectors

The routes that reach a subscription (ADR-0200): the Claude Code and Codex logins, OpenCode Zen
and Go, ClinePass, and the GLM and Kimi coding plans. Nothing of them is in p1's core crates
(ADR-0214): their logins are credential modules (ADR-0212), their usage probes are usage-probe
modules, and their dialects and client identity are data of their route files. This directory
holds everything the connectors need to build, test and package.

- `routes/`: one file per route, each with an inline `[credential]` (its implicit account,
  ADR-0139 §6). The store entry keeps the route id, so existing `p1 login` entries keep working;
  `glm-messages`, `opencode-go-messages` and `opencode-go-glm` reuse their sibling route's entry
  through `credential_route` (ADR-0134). Further accounts on these routes are account files of the
  user or of a pack. `anthropic-subscription` and `openai-codex-subscription` declare their
  dialect (`[adapter_settings.dialect]`) and `opencode-zen` its client identity
  (`[adapter_settings.client_identity]`).
- `profiles/`: the profiles only these routes bind.
- `logins/`: the crate `p1-subscription-logins`, the Claude Code, Codex, OpenCode and Pi login
  formats as the pure functions of the `credential-method` world (ADR-0212).
- `usage/`: the crate `p1-subscription-usage`, the Claude, Codex, OpenCode Go, Kimi and GLM usage
  endpoints and response decoders as the pure functions of the `usage-probe` world (ADR-0214).
- `components/`: one module package per crate, built with the SDK: the credential methods
  `connectors/claude-code-oauth`, `connectors/codex-oauth`, `connectors/opencode`,
  `connectors/pi` and the usage probes `connectors/claude`, `connectors/codex`,
  `connectors/opencode-go`, `connectors/kimi`, `connectors/glm`. Each is installed under the name
  after its namespace, the name routes and accounts already spell.
- `tests/`: the crate `p1-subscriptions-tests`, the suites that drive the logins through
  `p1-auth`'s credential chain and the probes through `p1-usage`'s ledger, natively
  (`cargo test --manifest-path subscriptions/tests/Cargo.toml`). It is a workspace of its own and
  takes p1's host crates from phaseone at the revision the root `Cargo.toml` pins the SDK at.

The guest crates are a workspace of their own (the root `Cargo.toml`), which names phaseone only
through `p1-module-sdk`, pinned there once. `./release.sh` builds every package with phaseone's
own build steps and packs it; a p1 distribution composes a tagged release of this repository
(phaseone's `connectors.lock`, ADR-0215): its share carries these routes and profiles, and its
release set these packages under their own namespace. The route files are composed and pinned by
that distribution's suites (phaseone's `crates/p1-host/tests`).
