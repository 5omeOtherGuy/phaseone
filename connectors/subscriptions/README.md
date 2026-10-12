# Subscription connectors

The routes that reach a subscription (ADR-0200): the Claude Code and Codex logins, OpenCode Zen
and Go, ClinePass, and the GLM and Kimi coding plans. The code that serves them stays in the core
crates behind the default-on cargo feature `subscriptions`; this directory holds their data and
their own checks.

- `routes/`: one file per route, each with an inline `[credential]` (its implicit account,
  ADR-0139 §6). The store entry keeps the route id, so existing `p1 login` entries keep working;
  `glm-messages`, `opencode-go-messages` and `opencode-go-glm` reuse their sibling route's entry
  through `credential_route` (ADR-0134). Further accounts on these routes are account files of the
  user or of a pack.
- `tests/`: the crate `p1-subscriptions-tests`, the suites that compose these route files through
  the host (`cargo test -p p1-subscriptions-tests`). It shares p1-host's test harness by path.

A build with the feature anchors these routes' origins (ADR-0196) and a release packs this root
next to the repository root; a build without it refuses every route here, naming the connectors.
