# Subscription connectors

The routes that reach a subscription (ADR-0200): the Claude Code and Codex logins, OpenCode Zen
and Go, ClinePass, and the GLM and Kimi coding plans. Their logins are credential modules
(ADR-0212); their usage probes and provider dialects still live in the core crates behind the
default-on cargo feature `subscriptions` (#889). This directory holds their login formats, their
data and their own checks.

- `routes/`: one file per route, each with an inline `[credential]` (its implicit account,
  ADR-0139 §6). The store entry keeps the route id, so existing `p1 login` entries keep working;
  `glm-messages`, `opencode-go-messages` and `opencode-go-glm` reuse their sibling route's entry
  through `credential_route` (ADR-0134). Further accounts on these routes are account files of the
  user or of a pack.
- `logins/`: the crate `p1-subscription-logins`, the Claude Code, Codex, OpenCode and Pi login
  formats as the pure functions of the `credential-method` world (ADR-0212). The components
  `modules/p1-module-claude-code-oauth`, `-codex-oauth`, `-opencode` and `-pi` export it with the
  SDK; its own suites drive it natively through `p1-auth`'s credential chain
  (`cargo test -p p1-subscription-logins`).
- `tests/`: the crate `p1-subscriptions-tests`, the suites that compose these route files through
  the host (`cargo test -p p1-subscriptions-tests`). It shares p1-host's test harness by path.

A build with the feature anchors these routes' origins (ADR-0196) and a release's distribution
packs this root next to the repository root (`stage-release.sh --parts subscriptions`,
ADR-0207); a build without it, like the release's bare product, refuses every route here,
naming the connectors.
