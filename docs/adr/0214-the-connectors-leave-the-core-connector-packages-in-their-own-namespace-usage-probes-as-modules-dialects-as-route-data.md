---
adr: 214
title: The connectors leave the core: connector packages in their own namespace, usage probes as modules, dialects as route data
status: accepted
date: 2026-10-12
deciders: owner
supersedes: []
superseded_by: []
sources: [ADR-0039, ADR-0067, ADR-0071, ADR-0086, ADR-0110, ADR-0139, ADR-0196, ADR-0200, ADR-0207, ADR-0209, ADR-0210, ADR-0211, ADR-0212, issue #886, issue #889, issue #893]
---
# ADR-0214: The connectors leave the core: connector packages in their own namespace, usage probes as modules, dialects as route data

## Context

Owner, 2026-10-11, for wave 3 of the repository split (#886): "I want modules developed outside
this repository. The whole idea is that everybody can develop modules." ADR-0211 gave the
subscription connectors' code its ports, and ADR-0212 made their logins credential modules.
Three pieces were still connector code in the core crates, behind the cargo feature
`subscriptions` (ADR-0200):

- the usage probes of the Claude, Codex, OpenCode Go, Kimi and GLM plans, compiled into
  `p1-usage`;
- the subscription dialects of the provider crates (`claude-code-subscription`,
  `codex-subscription`) and the OpenCode client identity of the Chat adapter;
- the four credential components, built in p1's own module workspace as `p1/` packages.

#889's last part moves them out: the connectors become crates and packages built with the SDK,
in a layout that `git subtree split --prefix=connectors` (#893) turns into a repository of its
own, and the feature goes away. The lead decided for the owner, 2026-10-12, under the
constraints of ADR-0212: a probe module only decodes response bodies, and the host attaches the
token natively.

## Decision

- **A module class `usage-probe`** with the world `p1:module/usage-probe` in WIT 1.2.0 (an
  additive minor, ADR-0210; SDK 1.2.0 with the feature `usage-probe`). It exports `describe`
  (the endpoint: url, credential method, static headers, account-id header, whether p1's
  `user-agent` is sent, whether a borrowed credential may reach it) and `decode(body)` (the plan,
  windows, credits and extra usage of a 2xx body), and imports nothing: its allocation is empty.
  Shapes: `docs/design/modules/usage-probe.md`.
  - The module never receives a credential. `p1-usage`'s `HttpProbe` builds the request,
    attaches the credential, maps a non-2xx status, and checks every decoded string again (a
    label, an RFC 3339 instant, a currency, a detail) before it is shown.
  - The probe's origins receive the account's credential, so they are the package's
    `token_origins`, as a credential method's refresh origins are (ADR-0212): shown at
    `p1 modules install`, installed only with `--confirm-origins`, pinned in the lock. The host
    refuses a description whose url is not https at one of them.
  - A credential borrowed from another tool's login reaches a probe only at an origin the
    compiled anchor ships for its method (ADR-0110), and only when the probe says a borrowed
    login may reach it: the module's `borrowed` flag can narrow that bound, never widen it.
  - `p1_usage::ProbeModule` / `ProbeModules` are the port; `p1-host`'s `InstalledProbes`
    composes the installed ones. An account's `usage` names an installed probe; a name no probe
    answers loads, and the account probes as unsupported ("usage probe `x` is not installed").
- **Dialects beyond the core's are route data.** A Messages or Responses route names a core
  dialect (`dialect = "api-key"`) or declares its own as a table (`[adapter_settings.dialect]`,
  `Declared`): a name and the facts the dialect trait asks (Messages: `claude`,
  `credential_header`, `identity`, `betas`, `client_headers`; Responses: `path`,
  `account_id_header`, `output_cap`, `client_headers`, `sse_headers`, `session_headers`). A Chat
  client identity is always declared (`[adapter_settings.client_identity]`: `name`, `headers`,
  and `ids` derived from the cache key as `prefix` and 26 hex digits). The connectors' route files
  declare `claude-code-subscription`, `codex-subscription` and `opencode` with the bytes the
  compiled dialects sent. The provider crates keep only the core dialects and the mechanism.
- **The connectors' layout.** `connectors/` is a guest workspace (`connectors/Cargo.toml`) that
  names phaseone only through `p1-module-sdk`, in one `[workspace.dependencies]` line:
  `subscriptions/logins` and `subscriptions/usage` (the formats and decoders, `serde_json` only)
  and `subscriptions/components/*` (one package each). The host-side suites
  (`subscriptions/tests`) are a member of the root workspace, which excludes the rest of
  `connectors/`.
- **Their namespace is `connectors/`.** The packages are `connectors/claude-code-oauth`,
  `codex-oauth`, `opencode`, `pi` (credential methods) and `connectors/claude`, `codex`,
  `opencode-go`, `kimi`, `glm` (usage probes); each is installed under the name after its
  namespace, the name routes and accounts already spell, so no route, account or store id
  changes (ADR-0139 §6).
- **The distribution installs them into the release set.** `scripts/build-modules.sh --all`
  builds them in the connectors' workspace, by the external-package checks (never `p1/`), into
  p1's build outputs; `scripts/stage-release.sh` ships a part's packages only with its part
  (`--parts subscriptions`), so the bare share lacks them. A name resolves to a lock entry of the
  class, else to the release set's package of the class named `<namespace>/<name>`, verified by
  the release manifest's digest, as p1's own release packages are; two release packages that
  claim one name are refused.
- **The feature `subscriptions` is gone** from `p1-host`, `p1-usage`, the provider crates, the
  module provider crates and the test crates, and with it `WITHOUT_SUBSCRIPTIONS`, the gate's
  `bare` job and `scripts/bare-smoke.py`. The build anchors the core and `connectors/subscriptions`
  when `P1_CATALOG_ROOTS` is unset; a release names its roots explicitly.

## Consequences

- Nothing of the connectors is in a core crate. A plan's probe, a subscription's wire and a
  login's format change in `connectors/` alone; the core keeps the mechanism and its checks.
- The invariant ADR-0212 narrowed is kept for probes: no module receives a request-path token.
  New residual risk: a usage-probe package decides where an account's credential is sent, bounded
  by the origins the owner confirmed at install; the release's own probes are pinned by its
  manifest.
- A route file may now declare a dialect, so a third party can reach a vendor's account with the
  core adapters and data alone. The adapters refuse a declared header they or the broker set and
  any secret-shaped name, by one rule (`p1_provider_http::declarable_header`).
- This changes two spellings earlier records decided, and nothing else of them: ADR-0067's
  `client_identity = "opencode"` names no compiled identity any more (the route declares the
  identity's headers and ids), and ADR-0039's "named by behaviour, compiled, never free-form
  headers" for the subscription dialects becomes "a core dialect by name, or a dialect the route
  declares and the adapter checks". An old name-only spelling is refused with a message naming
  the connectors' route file that declares it.
- The share gains no new member: an installer that predates this record installs a share that
  carries it, so the owner's updater keeps working.
- `p1 env show claude2`, `p1 usage` and `p1 login --list` resolve the connectors through the
  release set exactly as a session does; `p1_host::auth::credential_source` composes the
  installed credential modules too (the live smoke tests use it).
- The connectors' crates still compile against this checkout's SDK by path; after #893 the one
  line names the `module-sdk-v1.2.0` tag. Their host-side suites need phaseone's host crates
  (`p1-host`, `p1-auth`, `p1-usage`, `p1-module-tests`, the provider crates); #893 decides how a
  connectors repository fetches them.
- Pushing tag `module-sdk-v1.2.0` on the merge commit releases the SDK with the new world.

## Alternatives considered

- **Connector provider modules** built on the provider crates. Rejected: a connector would link
  phaseone's adapters, and every dialect would be a copy of a provider component.
- **Probes as data** (a url and a field map). Rejected: the Kimi detail matching and the GLM
  window kinds are logic, and a decoder in a module costs nothing more.
- **A shipped lock for the connectors' packages** (`share/p1/modules.lock`). Rejected: the
  installer in the owner's prefix refuses any share member it does not know, so the next update
  would fail; the release set already pins its packages by digest.
- **Keeping the feature with the code moved.** Rejected by #889: a cargo feature cannot be
  installed.

## Evidence

- `cargo test -p p1-usage`: the ledger over fake probe modules (origins, credentials, echoes,
  declared origins and headers).
- `cargo test -p p1-subscriptions-tests --test usage_probes`: the five decoders, natively,
  through `p1_usage::decode_usage`, with the fixtures the compiled probes were pinned by.
- `crates/p1-module-tests/tests/usage_probe_modules.rs`: the built probe components through the
  ledger, their origins confirmed at install, an importing component refused.
- The provider crates' suites over declared dialects carrying the subscription's facts, and the
  connectors' route suites over the shipped route files.
- `scripts/test_stage_release.py`: a connector package ships only with its part.
