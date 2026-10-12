---
adr: 217
title: One repository: connectors and kits are packs, the model is the selector, one release
status: accepted
date: 2026-10-12
deciders: owner
supersedes: [200, 207, 215]
superseded_by: []
sources: [owner words 2026-10-12 (this record's Context), https://github.com/5omeOtherGuy/phaseone/issues/924, issue #840, issue #886, issue #893, ADR-0110, ADR-0153, ADR-0196, ADR-0197, ADR-0200, ADR-0201, ADR-0206, ADR-0207, ADR-0209, ADR-0210, ADR-0212, ADR-0214, ADR-0215, docs/design/model-selection.md, docs/design/prompts.md, docs/design/seams.md, GPT 6 Astra review of the skeleton 2026-10-12]
---
# ADR-0217: One repository: connectors and kits are packs, the model is the selector, one release

## Context

The repository split of 2026-10-11 and 2026-10-12 (#840 waves 1 and 2, #886 wave 3, #893) did
not land where the owner meant it to. Owner, 2026-10-12: "The recent split didn't turn out the
way I expected. There were some misunderstandings." And what was meant: "the whole idea was to
have it customized for the models or the providers. Using ChatGPT means the ChatGPT models use
different tools than Anthropic models. They have a different type of compaction that works. They
have a different type of prompt caching. I want each of them to be able to be optimized, and also
for the user to take it and optimize it. Everyone can install what they want, but they also can
install a ChatGPT bundle. And the moment you use GPT 6.1, the other tools are deactivated. It
only uses the compaction that fits this model, the type of communication that fits OpenAI; it can
switch to WebSocket while Anthropic is using HTTPS."

What the split did, from the record. #840 (owner, 2026-10-11): one repository, the core as the
bare product, connectors and the owner pack as optional parts (ADR-0200, ADR-0201, ADR-0207).
#886 (owner, 2026-10-11, "I want modules developed outside this repository. The whole idea is
that everybody can develop modules"): external packages by digest (ADR-0209), the module SDK
(ADR-0210), credential-method and dialect ports (ADR-0211), logins as modules (ADR-0212), the
connectors as packages with usage probes as modules and dialects as route data (ADR-0214). #893
(owner, 20:59Z the same evening, "unit 5, last"): the connectors moved to the repository
phaseone-connectors by subtree split, and phaseone pins one release of it by digest
(ADR-0215). The lead's split handoff of 2026-10-11 §2.1 had advised against that last step:
the connectors are tied to the compile-time credential anchor. The proof #893 was for was
made: a connector change (v0.1.1) landed with no phaseone pull request and installed into a
released p1 with `p1 modules install`. That proof holds through the SDK and the external
package path, and does not need the first-party connectors to live outside. What the separate
repository left behind: a network fetch in every distribution build, a lock-bump pull request
for every connector change, two places to look, and #920 (the SDK tag is unusable as a git
dependency).

"Subscription" was never a category. Of the ten connector routes, two are logins borrowed
from a vendor's CLI (Claude Code, Codex); eight are API keys to coding-plan endpoints (Z.ai,
Kimi, OpenCode, Cline).

Where the variation the owner names lives today, checked at 969bbc0f:

- The wire varies by vendor and is already per vendor: transport (`transport = "websocket"`
  with SSE fallback on the Codex Responses route, ADR-0047; the public Responses route is SSE
  only), prompt caching (Anthropic cache markers in the Messages dialect, `prompt_cache_key` in
  the OpenAI adapters), dialect, retry policy, login method and usage probe are route and
  module data (`routes/*.toml`, `crates/p1-provider-*`, the credential and usage-probe
  modules).
- Model limits vary by model and are already per model: `profiles/<model>.toml` carries the
  context window, the thinking policy, the efforts and a `family` key. What a route serves of
  a model (the Codex subscription's 272k window against the public API's) is wire data too: a
  route's model binding has a `context_limit` field that nothing consumes yet
  (`crates/p1-host/src/routes.rs`).
- The agent's way of working varies by model family and is NOT keyed by it: the tool set
  (the Codex pair `shell` + `apply_patch` for GPT, `read`/`edit`/`write`/`grep` for Claude),
  the prompt, `[context]` (the compaction thresholds and the summarizer override),
  `[tool_concurrency]` and `[options]` live in the environment, by hand, copied once per route.
  `packs/owner/README.md` calls `glm`, `glm-go` and `glm-messages` "three identical copies",
  and `deepseek`, `deepseek-messages` and `cline` the same. The only inheritance is `alias_of`
  (`crates/p1-assembly/src/lib.rs`), which changes the account and nothing else. The one
  piece that already resolves by family is the prompt: `docs/design/prompts.md` (#779) takes
  the model variant of a fragment, else the family variant, else the base.
- Compaction is one implementation: the host composes the summarizing context module
  whenever an environment has `[context]`, and a model switch keeps the generation's context
  module (`crates/p1-host/src/run.rs`, "A switch keeps the generation's modules").
- The environment is the selector and the model a field in it: an environment "fixes the
  prompt family, the tools and `[context]`" (`docs/design/model-selection.md` §1), and a bare
  `/model P` resolves only inside the current environment's route, so switching from `claude`
  to a GPT model needs `gpt/gpt-6-sol` and a hand-made `gpt` environment.

The dsh floor (ADR-0153) for the capability "product composition" (seam catalog, family 1):
dsh composes a product as `dsh-base` plus mode bundles plus the user's patch over an empty
root, a patch replaces a row whole and never merges, and the vendor wire is one package per
vendor and credential mode (`dsh-llm-deepseek-api-key`, `dsh-llm-deepseek-account`), which
"never fall back to each other". p1 restates everything per environment; that row is the gap.

## Decision

The model is the selector. A user picks a model, and p1 derives the wire that reaches it and
the way of working that suits it; both are packs a user can install, copy and tune.

1. **One repository.** The connectors return to phaseone and phaseone-connectors is archived
   with a pointer here. `connectors.lock`, `scripts/fetch-connectors.py` and the fetch in
   `scripts/build-modules.sh --all` go. Every first-party module and route is built from this
   repository; a third party's module is built with the SDK (ADR-0210) and installed by digest
   (ADR-0209), unchanged.
2. **Two shipped kinds: connectors and kits.**
   - A **connector** is one vendor's wire under `connectors/<vendor>/`, laid out as a pack
     root (ADR-0201) and a catalog root of the build: `routes/` (its routes, API key and login
     alike; one route per wire and credential mode, as today), `profiles/` (only a variant
     that exists for this vendor alone, such as `glm-5.3-flash-clinepass`; the canonical
     first-party profiles stay in `profiles/` at the repository root), `components/` (the
     sources of its login and usage-probe modules) and a README naming its routes and modules.
     The sources build in the `connectors/` guest workspace (ADR-0214), discovered as
     `connectors/*/components/*/` as they were at 9c204ee3, packages in the `connectors/`
     namespace, shipped in the release's module set. The login-format and decoder crates and
     the host-side suites keep a home under `connectors/` (per vendor or shared; the suites
     stay a member of the root workspace); the regroup unit decides the exact paths. A
     connector a user installs as a pack carries, beside its routes, an installed set
     `modules/` and a `modules.lock` (ADR-0209). A connector says nothing about how an agent
     works. The vendors: `anthropic`, `openai`, `zai`, `opencode`, `kimi`, `cline`, and `pi`,
     the one exception: a credential source (a login borrowed from the pi CLI) with no route
     of its own. The root `routes/` dissolves into `connectors/anthropic` and
     `connectors/openai`.
   - A **kit** is one model family's way of working, and is catalog content, not a root:
     `kits/` is a catalog sibling like `routes/`, so a kit lives at
     `<root>/kits/<family>/`, in the root catalog for the shipped ones, under `<pack>/kits/`
     in a pack and under `~/.config/p1/kits/` in the user directory, and the first file per
     relative path wins whole (ADR-0201). The family is the `family` key of the model profile;
     a family without a kit uses `base`. A kit holds `kit.toml`, `<profile-id>.toml` overrides
     for one model (the profile id, never a wire id), `summarize.md`, and, once #779 has
     landed, the family's and its models' prompt variants under `prompts/`; the role profiles
     of `docs/design/prompts.md` (`main`, `fable`, `manager`) stay shared under
     `environments/prompts/`. The owner's "ChatGPT bundle" is therefore one pack root holding
     `routes/`, `kits/gpt/`, `profiles/` and `modules/`; the shipped `openai` connector and
     the shipped `gpt` kit are its first-party form.
   - **Kit tables.** A kit carries every table an environment carries today except `route`,
     `profile`, `account` and `[[mcp_servers]]`, which stay environment-only: `[[tools]]`,
     `prompt`, `[context]`, `[tool_concurrency]`, `[options]`, `[instructions]`, `[skills]`,
     `[capabilities]`. `[context]` holds the family's compaction preferences and a `policy`
     key naming the context-policy module (the summarizing one by default); the served
     capacity (window and output limits) is wire data from the profile and the route's model
     binding, which the kit unit makes effective. A role environment (a reviewer, a finder)
     stays an environment: its restricted tools and prompt replace the kit's.
   - **Resolution.** For each table: the profile's override file, else the family's
     `kit.toml`, else `kits/base/kit.toml`; each file is found across the layers by the
     ADR-0201 rule, specificity before layer priority; a table is taken whole, never merged,
     and the keys a replacing table leaves out take their defaults (`[tool_concurrency]`
     without `shell_reads` means `shell_reads = true`). A user who tunes one table copies the
     whole kit file, as one copies a whole environment today. An environment's own tables
     replace the kit's the same way. A kit names only module keys the lock resolves; nothing
     registers itself. `description_file` and `summarize.md` resolve relative to the file that
     names them.
3. **A thin environment, and the model as selector.** An environment is `model = "<profile>"`,
   optional `route` and `account`, its `[[mcp_servers]]`, and the tables it changes. A model
   choice (`--model`, `/model`, a worker's model) resolves its route in this order: an
   explicit `E/P` names the environment; else the current route and account when they bind
   the profile; else the route `settings.toml` prefers for the profile, else for its family;
   else the sole shipped or installed route that binds it; else the choice is refused naming
   the candidates. The family picks the kit. `:effort` and `@account` as in
   `docs/design/model-selection.md`; the `route` + `profile` form keeps working. A switch at
   a turn boundary changes tools, prompt, context policy, options and the wire through
   `Agent::reconfigure`; the context module is part of the switch. The shipped `default`
   stays a thin environment that names its route, because two Anthropic routes bind its model
   once the connectors are back. A connector or kit pack that wants a route chosen makes that
   preference explicit in its README or in settings; nothing is inferred from directory order
   or credential presence.
4. **One release.** A release publishes one composition under the existing asset names
   (`p1-linux-x86_64`, `p1-share.tar.gz`); the bare composition, `p1-bare-*`, `install.sh
   --bare` and the `.p1-assets` marker go. The binary anchors the root catalog and every
   shipped connector (ADR-0196) from one explicit list of roots (`.` and each
   `connectors/<vendor>`; `build.rs` reads only a root's immediate `routes/` and `accounts/`
   and refuses an empty route set) that `build.rs`, the debug catalog lookup,
   `stage-release.sh`, `release.yml` and the gate read. `kits/` becomes a share member:
   `catalog-roots.py` merges it, `stage-release.sh` archives it, and `install.sh` accepts it.
   Since the installed updater keeps its installer script and today's installer refuses any
   member outside `environments`, `routes`, `profiles`, `accounts` and `modules`, the kit unit
   lands over two releases: first the installer that accepts `kits/`, then the first share
   that carries it. The owner pack is in neither the release nor the anchor: it is installed
   as a pack (`packs` in `settings.toml` or `P1_PACKS`), and its accounts are user-layer
   accounts.
5. **What stays.** p1's own modules under `modules/` (flat `p1-module-*`); the owner pack in
   this repository with the pack-only CI path (ADR-0206); ADR-0200's boundary rule (an
   optional part uses only interfaces another repository could use; nothing in the core names
   a connector's, a kit's or the pack's ids); the compile-time anchor for borrowed logins
   (ADR-0110): a third-party connector cannot borrow a vendor CLI's login unless its origin is
   compiled in, and a run-time rung for that is a later record if wanted. Front ends stay
   compiled crates behind the front-end port (ADR-0152); product modes (cli, acp, headless) as
   bundles of defaults are a later record.

## Consequences

- The owner's "ChatGPT bundle" is the `openai` connector plus the `gpt` kit. A pack directory
  is installed through `packs` or `P1_PACKS` today; a command that installs a pack archive is
  part of the kit unit or a follow-up (today `p1 modules install` installs one package into the
  user's set).
- The owner pack's twenty-odd environments shrink to a line or two each; `glm`, `glm-go` and
  `glm-messages` become three thin environments over one kit; `deepseek-review` keeps its own
  tools and prompt as an environment over the `deepseek` kit.
- Today's `gpt` environment carries the Codex subscription's served window in `[context]`;
  with the split of Decision 2 that number moves to the Codex route's model binding, and the
  kit keeps the preferences. Until the kit unit makes `context_limit` effective, "the
  compaction that fits this model" is the thresholds and the summarizer text; a different
  compaction implementation per family needs the `policy` key and a switch that replaces the
  context module.
- A connector change is a phaseone pull request again; the gate builds and tests the
  connectors; no build needs the network beyond crates.
- The owner pack leaving the anchor: a borrowed login reaches the compiled origins of its
  method regardless of the id that names it (ADR-0110), and the vendor's connector compiles
  those origins, so `claude-2` keeps working; a store-only account needs its origin recorded in
  the store, which `p1 login` does. Unit 6 of #924 checks `p1 login --list` for every owner
  account before the pack leaves.
- The kit unit amends `docs/design/prompts.md`: the kit's fragment paths, the shared role
  profiles, lookup across pack layers and compatibility with a user's existing overrides.
- The seam catalog row "Product composition" changes status in the kit unit; the row for
  model selection in the unit that resolves a bare model.
- Six units (#924); until the kit unit lands, environments stay complete orders. Nothing in
  this record changes code; the skeleton directories carry a README each, and p1 reads
  nothing from them.
- #920 loses its first-party user; the template module still needs the fix.
- ADR-0200, ADR-0207 and ADR-0215 are superseded. What survives of them is restated in
  Decision 5; the pack layering (ADR-0201), the anchor roots (ADR-0196), the core routes and
  default environment (ADR-0197) and the pack-only CI path (ADR-0206) are unchanged.

## Alternatives considered

- **Keep the separate connectors repository** (ADR-0215): rejected by the owner on 2026-10-12;
  the proof it made survives through the SDK and the external package path.
- **Regroup `modules/` by kind** (providers, logins, usage, tools, policies) and merge the
  catalog into `catalog/` (the other session's proposal of 2026-10-12): rejected. Eighteen
  scripts, workflows and build files glob the current paths, the regroup changes no behaviour,
  and grouping by kind spreads one vendor over four directories, the opposite of a bundle.
- **One route per vendor listing its credential options** (same proposal): rejected. A route
  has one credential method and one dialect, and the Anthropic key route and the Claude Code
  route differ on the wire (identity block, betas, headers), so merging them means the dialect
  must follow the credential, a format change in `routes.rs` and `build.rs`; dsh keeps two
  route plug-ins that never fall back to each other, which is p1's shape today.
- **A kit tied to its vendor** (a ChatGPT bundle that owns the GPT behaviour): rejected. GLM
  reaches p1 through three routes, DeepSeek through three, Claude through two; the identical
  copies would return.
- **A kit directory as a pack root of its own** (the skeleton's first draft): rejected on
  review; the pack search builds `<root>/environments` and looks for siblings beside it, so a
  family directory given to `P1_PACKS` would find nothing. A kit is content under `kits/`.
- **Refuse a bare model whenever several routes bind it** (the first draft of Decision 3):
  rejected on review; two routes bind every Claude model and three bind GLM, so the ordinary
  switch would fail. The current route, then an explicit preference, then the sole candidate.
- **Field-merging inheritance** for kits and environments: rejected; whole-table replace is
  what dsh does and is easier to read back from the files.
- **Front ends as WebAssembly modules**: not possible today (no WIT class; a front end owns the
  terminal and the process); compiled crates behind the port, selected by a product mode.

## Evidence

- Owner words of 2026-10-12 quoted in Context; #840, #886 and #893 with their comments;
  the lead's `~/.agents/xo/dispatch/p1-lead-20261004/handoff-repo-split.md` §2.1 (2026-10-11)
  and `NOTES.md` 20:59Z 2026-10-11 ("connector repo split = unit 5 (last) of wave 3").
- Facts checked at 969bbc0f on 2026-10-12: `connectors.lock` pins phaseone-connectors v0.1.0;
  `scripts/build-modules.sh` fetches it; `crates/p1-host/build.rs` anchors `.` and the fetched
  catalog, reads a root's immediate `routes/` and `accounts/` and asserts "no shipped routes
  to anchor credentials"; `scripts/install.sh` allows the share members `environments`,
  `routes`, `profiles`, `accounts`, `modules`; `.github/workflows/release.yml` builds the
  distribution and the bare binary; `crates/p1-host/src/run.rs` holds the one front-end
  branch point and keeps the context module across a switch; `crates/p1-host/src/models.rs`
  builds `<pack>/environments` for each pack root; `crates/p1-assembly/src/lib.rs` has
  `alias_of` as the only inheritance; `crates/p1-host/src/routes.rs` `ModelBinding.context_limit`
  unconsumed; `packs/owner/README.md` "three identical copies"; `docs/design/model-selection.md`
  §1 (bare `P` inside the current environment); `docs/design/prompts.md` (model, family, base);
  `profiles/*.toml` `family`; `routes/openai.toml` and `crates/p1-provider-openai/src/lib.rs`
  (WebSocket on the Codex route, SSE only on the public one);
  `crates/p1-provider-openai/src/request.rs` `prompt_cache_key`; the ten connector routes'
  `[credential]` tables (two logins, eight API keys); DeepSeek bound by
  `opencode-go-subscription`, `opencode-go-messages` and `cline-pass`.
- phaseone-connectors releases v0.1.0 and v0.1.1; #893's closing comment (v0.1.1 installed into
  a released p1 with `p1 modules install`, a connector change without a phaseone pull request).
- dsh survey of 2026-10-10, family 1 (product composition, patch semantics) and family 5
  (one package per vendor and credential mode), at the location ADR-0153 names.
- GPT 6 Astra's read-only review of the skeleton, 2026-10-12, on the owner's request: twelve
  findings, folded into Decisions 2 to 4 and the two rejected drafts above.
- Acceptance of this record: `python3 scripts/adr.py check` passes; `connectors/` and `kits/`
  hold the skeleton READMEs; each unit of #924 carries its own evidence.
