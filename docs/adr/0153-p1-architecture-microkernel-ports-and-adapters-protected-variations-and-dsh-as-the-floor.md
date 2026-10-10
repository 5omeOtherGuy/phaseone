---
adr: 153
title: p1 architecture: microkernel, ports and adapters, Protected Variations, and dsh as the floor
status: accepted
date: 2026-10-10
deciders: owner
supersedes: []
superseded_by: []
sources: [owner words 2026-10-10 (issue #129 review; issue #670 comment "Owner decisions 2026-10-10 01:5x"; question dialogs of ~13:3x recorded in ~/.agents/xo/dispatch/p1-lead-20261004/dsh-survey/DRAFT-NOTES.md "Owner decisions 2026-10-10 ~13:3x"; scope answers of the same afternoon, recorded there under "Owner scope answers 2026-10-10"), issue #129, issue #670, issue #697, issues #695, #696 and #709 to #717 (seam designs), ADR-0002, ADR-0003, ADR-0004, ADR-0017, ADR-0025, ADR-0032, ADR-0035, ADR-0039, ADR-0050, ADR-0053, ADR-0071, ADR-0080, ADR-0103, ADR-0118, ADR-0128, ADR-0151, ADR-0152, ADR-0154, ADR-0156, AGENTS.md, docs/design/seams.md, docs/design/pillars.md, dsh survey of 2026-10-10 (~/.agents/xo/dispatch/p1-lead-20261004/dsh-survey/SURVEY.md and family-*.md)]
---
# ADR-0153: p1 architecture: microkernel, ports and adapters, Protected Variations, and dsh as the floor

## Context

Terms used below, each glossed once: a **microkernel** is a core that does one thing and loads
everything else as a module; a **port** is a contract (a Rust trait and its value types) that
the core or the host calls; an **adapter** is a crate that implements a port; a **composition
root** is the one place that wires adapters to ports; a **registry** is a runtime table that
code looks things up in; **Protected Variations** (Larman, GRASP, after Parnas 1972
"information hiding") says: put an interface at a point you know will vary; **dsh** is the
DeepSeek harness (`deepseek-ai/deepseek-harness`, surveyed at `dsh-v0.2.0-rc.2`, commit
639ed01), an everything-is-a-plug-in coding harness and p1's best-practice reference.

Three things forced this record.

1. **The owner's decisions of 2026-10-10.** On the #129 skills design the lead had recommended
   one crate now and a split later; the owner rejected that: modularity is what p1 is for, and
   "we should never be worse at it otherwise => Why would anyone use us?!". Later the same day,
   on the ACP dependency choice (issue #670, comment of 2026-10-10 01:5x), the owner named the
   rules every p1 design option is judged by: "**microkernel** (core = agent loop only);
   **ports and adapters** (each variation is a port in p1-contracts with adapter crates behind
   it); **protected variations** (protect every known variation point now); **dsh is the
   floor** (p1 may be leaner or safer than dsh, never weaker)". Later that day (~13:3x, question
   dialogs with the lead) the owner narrowed what the floor means: "never worse than dsh was
   probably too hard a phrasing. We are talking about our current state of development. While
   we are laying the foundation, in regards to modularity, we do not want to be worse than
   dsh. What happens after that doesn't matter now." The floor is therefore the modularity of
   the seams p1 builds during the foundation phase, measured against dsh's default product
   (`dsh-base` and the packages it switches on); it is not feature parity, and dsh's
   experimental or off-by-default packages are outside it. The worked example was decided
   with #129: dsh has `dsh-skill` (registry and contract), `dsh-skill-filesystem` (disk source,
   the only YAML reader) and `dsh-tool-skill` (catalog text plus the `skill` tool); p1 gets
   skill types and a source trait in `p1-contracts`, the disk source `p1-skill-fs` (its own
   two-field front-matter reader, no YAML library) and `p1-tool-skill`; `p1-host` joins them
   explicitly (ADR-0151, landed with #700).
2. **The dsh survey.** On 2026-10-10 the lead commissioned a survey of every dsh package (289
   npm packages plus 46 source-only packages, 16 families) against p1. It produced 227 rows,
   one per capability. After the owner's decisions of ~13:3x the catalog's statuses were
   reworked and recounted by script (recounted after the 2026-10-10 rework): 73 `missing`,
   61 `partial`, 28 `built`, 31 `different-by-design`, 10 `in-flight`, 24 `optional` (the
   survey's own counts were 101, 63, 28, 25, 10 with no `optional` status); after the owner's
   scope answers of the same afternoon (Decision 9) and the records that landed on main meanwhile
   (ADR-0151, ADR-0152, ADR-0154, ADR-0156) it was recounted once more: 63 `missing`,
   59 `partial`, 31 `built`, 44 `different-by-design`, 7 `in-flight`, 23 `optional`. Each row was written
   by one mapper and checked by an independent verifier against cited evidence; family 14
   (client UI, 44 rows) was mapped and verified by a smaller model only. The survey is a
   hand-off, not a decision.
3. **The design drafts were never approved.** `docs/design/seams.md` (draft v1, "modules and
   seams") and `docs/design/pillars.md` (rev 2) were technical proposals. The rules that
   matter from them already live in accepted records: the core depends only on `p1-contracts`
   (ADR-0002, enforced by `scripts/check-core-isolation.sh`); modules are WebAssembly artifacts
   loaded by name from the environment file, composed at one root, with no service locator,
   global registry or auto-registration (ADR-0071, which
   superseded ADR-0004's compile-time rule; ADR-0103 removed the pointer-keyed `bound_tools`
   registry so that an assembled tool carries its own capability snapshot, while its fallback
   map stays process-global for test fixtures and standalone native tools; a dependency-injection framework is forbidden by `AGENTS.md` and
   ADR-0103).
   What was missing was one place that says, per capability, which port and adapters p1 builds
   and how far p1 stands from dsh.

## Decision

1. **p1's architecture is named with established terms.**
   - **Microkernel.** `p1-core` is the agent loop and its API only, and depends only on
     `p1-contracts` (ADR-0002). Providers, tools, context and authorization policies, journals,
     workers, workflows and front ends are modules around it.
   - **Ports and adapters.** A port is a contract in `crates/p1-contracts` (a trait plus its
     value types, for example `Provider` in `provider.rs`, `Tool` in `tool.rs`, `ContextPolicy`
     and `AuthorizationPolicy` in `policy.rs`, `CommitSink` in `journal.rs`); an adapter is a
     crate that implements it, natively or as a WebAssembly module crate under `modules/`.
     A port may take a second form at the module boundary: a WIT interface under `modules/wit/`
     with its host service trait in `p1-module-runtime` (for example `process-jobs` and
     `ProcessJobsService`). Both forms count as ports. A trait that lives only in `p1-host`
     (today `Asker`, `QuestionAsker`) is a host composition trait, not a port, until it moves
     to `p1-contracts`; ADR-0152 did this for the front end (`FrontEndPort` and
     `SessionHandle` in `contracts/frontend.rs`). `p1-host` is
     the composition root; it names every adapter it loads (ADR-0071) and nothing registers
     itself.
   - **Protected Variations.** A **known** variation point gets its port and its adapter
     crates in the first slice that touches it, even with one implementation. The test for
     "known" (owner 2026-10-10): a variation point is known when at least one
     of these holds: dsh has a seam for it (a contract package separate from its providers);
     an owner decision or an accepted ADR names it; p1 already has, or has decided on, a
     second adapter. Anything else is a speculative
     "evolution point" and waits; "no speculative abstraction" in `AGENTS.md` means exactly
     that, and nothing more.
2. **dsh is the floor, and the floor is modularity.** During the foundation phase, every seam
   p1 builds is at least as modular as dsh's equivalent in dsh's default product (`dsh-base`
   and the packages it switches on), surveyed at `dsh-v0.2.0-rc.2`; that tag is the reference
   for the foundation phase, and later phases are out of this record's scope (owner
   2026-10-10: "What happens after that doesn't matter now"). There is no re-check cadence.
   The floor is not feature parity: a feature p1 lacks is not below the floor. A feature p1
   builds with a weaker seam than dsh's is: fewer ports than dsh separates for it, a source
   fused with its consumer, or a registry-free composition that loses a variation point dsh
   protects. The floor is checked **per catalog row** when the row is built or changed (the
   catalog is Decision 4, its "Floor check" section): find the row, read the dsh port and
   adapters named in its "dsh counterpart" cell, and build at least the ports dsh separates.
   The floor is never judged by package count, by lines of code, or by copying dsh's
   mechanism: p1 may reach a capability with fewer parts, a narrower surface, or a stricter
   rule, as long as no variation point dsh protects is fused away; the catalog's "Better than
   dsh" cell records what p1 must not lose when it changes a row. Feature differences are
   recorded separately: the catalog's "Gap / next slice" cell and its ranked feature-gap
   table, weighed in the survey's order (owner 2026-10-10): safety, correctness of the work,
   ability to finish tasks, recovery from failure, cost. That ranking schedules work; it does
   not mark floor violations.
3. **Allowed deviations.** A row is `different-by-design` only when (a) an owner decision or an
   accepted ADR is cited in the row and (b) the reason is in the row. The recorded reasons are
   of four kinds: p1's composition rules (no registry, locator or auto-registration); a
   security rule stricter than dsh's (for example always-on workspace confinement, ADR-0025);
   a scope decision by the owner (ACP as the one door, no second automation protocol, no
   telemetry or upload because nothing leaves the machine, the direct DeepSeek API route only
   and no account service; Decision 9); a mechanism choice recorded in
   an ADR (for example rhai over JavaScript, ADR-0053). The owner authorised the lead
   (2026-10-10) to mark, alone and in one batch, rows where p1 plausibly wants nothing and no
   owner question is open; such a row cites `owner 2026-10-10: lead batch` with a one-line
   reason. A deviation with none of these citations is `missing` or `partial`, not
   `different-by-design`, and the row says that a decision is needed. A capability dsh ships
   off by default or as experimental is `optional`: outside the floor, neither a gap nor a
   deviation.
4. **The catalog.** `docs/design/seams.md` is rewritten as the seam catalog: one table per
   survey family with the columns Port, p1 contract (path), p1 adapters (crates), dsh
   counterpart, Status, Gap / next slice, Better than dsh; then the ranked feature gaps (not
   floor violations) and the floor check. It opens with how to use it. Its evidence stays in the survey, named by date and
   location; the catalog copies none. The lookup rule: no session asks "how does dsh do it?";
   it reads the feature's row and builds the port named there. A feature with no row gets a
   row first, through the lead. A row changes status in the pull request that changes the
   code. The survey's 14 owner questions are answered (Decision 9); a row an answer touches
   carries the answer, `owner 2026-10-10: plan (seam: #N)`, `owner: not now` or `owner: after
   ACP door`, and no row waits on an owner question.
5. **What this record does to the drafts.** `docs/design/seams.md` draft v1 is replaced in
   full by the catalog and kept verbatim as `docs/design/seams-v1.md`, so that the section
   references in accepted records (ADR-0002, ADR-0003, ADR-0017, ADR-0025, ADR-0032 and others),
   `STATUS.md`, `docs/SLICE-REPORT.md`, `docs/design/design-summary.md` and
   `scripts/check-core-isolation.sh` ("seams.md section 3/4/5/10/11", "§10") point to that file and no
   accepted record is edited; its dependency rules survive in `AGENTS.md` and ADR-0002, its §10
   acceptance criteria were met (`docs/SLICE-REPORT.md` "Acceptance (seams.md §10)", ADR-0002), and its §6 line "policy interfaces, not
   a general hook platform" is not carried forward as a decision (the hook rows are `optional`,
   since dsh ships no hook bridge in a bundle; the owner plans a hook core plus a Claude Code
   bridge, seam #711, Decision 9). `docs/design/pillars.md` is not ratified by this record: pillar 3
   (a small core with exchangeable WebAssembly modules) is already accepted through ADR-0002
   and ADR-0071; pillars 1, 2 and 4, the orchestration paragraph and the first-slice scope are
   product direction outside this record and keep their draft status line.
6. **The `AGENTS.md` Architecture rule** is amended in the same change: one line names the
   four rules, points at the catalog and states the lookup rule; the modularity line names the
   document that says what varies (the catalog plus accepted ADRs); "no speculative
   abstraction" is glossed as "an abstraction no part of the design calls for", and a module
   split the catalog or an accepted ADR calls for is required, not speculative.
7. **Relation to accepted records.** This record builds on ADR-0002 (one small core), ADR-0071
   (modules by name, one root, no registry), ADR-0103 (no process-global registry), ADR-0039
   (a provider is composed from a wire adapter, a route and a model profile: an early port
   with three adapters), ADR-0050 (a worker gets exactly the tools its parent grants), ADR-0053
   (workflows as an optional module), ADR-0118 (concurrency as a per-call tool property, an
   example of extending a port without naming an adapter in the core) and ADR-0128 (how work
   moves: a ten-line brief names the issue and its owned paths, and the catalog row is what the
   issue points at). It supersedes none of them.
8. **What stays deferred.** The owner answered the survey's 14 questions on 2026-10-10
   (Decision 9). What remains deferred: a GUI and voice (after the ACP door works); browser and
   computer use, agent teams and Office documents (not now); and the implementation of every
   planned seam (designed, not ordered). The order in which the feature gaps are closed is the
   lead's scheduling, not this record; the ACP epic (#670, with the dsh-floor children #691 to
   #696; #673, #690 and the front-end port #697 landed as ADR-0154, ADR-0156 and ADR-0152) is
   already ordered. Moving host-side ports (the file, process and jobs capability traits in
   `p1-module-runtime`; the worker service in `p1-workers`) into `p1-contracts` is decided per
   row when its second adapter is known, not wholesale (owner 2026-10-10); the SSH and
   external-worker seams (#713, #715) are such decided second adapters, and their ports move in
   the slice that builds them. No crate is created by this record. What happens after the
   foundation phase is out of scope.
9. **The owner's scope answers, and what "plan" means (owner 2026-10-10, afternoon).** The
   survey's 14 questions are answered as follows; the catalog's closing table repeats them, and
   each touched row carries its answer. **Plan:** operating-system adapters for Linux, macOS and
   Windows (#709); image and file input (#710); a hook core with a Claude Code bridge, the Codex
   bridge optional (#711); the direct DeepSeek API route only (#712); SSH remote execution
   (#713); programmatic tool calling (#714); external-harness workers, that is Claude Code, Codex
   or any ACP agent on the worker seam (#715); scheduled runs and webhook-started runs (#716);
   web search and fetch (#717); the MCP client (design on #695); sandbox per-command escalation
   that the user approves, a read-only mode and an unsandboxed "dangerously skip permissions"
   mode (design on #696). **Not now:** browser use and computer use; agent teams; Office
   documents. **After the ACP door works:** a GUI; voice. **Never:** telemetry, feedback
   collection and session upload, because no data leaves the machine; those rows are
   `different-by-design`, as are DeepSeek account sign-in, balance and log upload. "Plan" is the
   owner's word: "Plan it does not necessarily mean build it; it can mean plan it so we can
   easily implement it later." A planned capability gets its seam designed now, in the catalog's
   "Planned seams" section and in one unassigned seam-design issue (title
   `seam: <capability> (plan, ADR-0153)`; for #695 and #696 a comment): the port it hangs on (an
   existing one, or a NEW contract or WIT interface, named as new), the interface shape in prose
   plus a short Rust or WIT sketch, the dsh packages it must match in modularity, and what a
   later adapter must supply. No implementation is ordered, no trait is added to the code by
   this record, and a build order is a separate issue that cites the design. Implementing a
   planned seam later is then a new adapter, not a rewrite.

## Consequences

- Every new feature starts at its catalog row; a worker brief names the row, the port and the
  adapter crates; "how does dsh do it?" becomes a lookup, and a missing row is raised with the
  lead before code is written.
- First slices carry more crates than before (a port in `p1-contracts`, one adapter crate, one
  model-facing crate, joined in `p1-host`); the owner accepted the build-time cost with #129.
- The catalog is maintained: the lead owns rows, a pull request that changes a capability
  changes its row, and the survey files outside the repository stay the evidence until a row is
  re-surveyed. `docs/design/README.md`'s one-line description of `seams.md` is updated when
  this lands.
- Feature gaps, recounted after the owner's scope answers of 2026-10-10: 129 rows carry one
  (63 `missing`, 59 `partial`, 7 `in-flight`); 21 of the 31 `built` rows also list remaining
  feature differences in their Gap cell. None of these is a floor violation. The catalog ranks the 29
  the survey weighed highest (rank 20, hook protocol core, became `optional`); this record
  promises no date. Whether a row built before this record meets the modularity floor was not
  surveyed; it is checked when the row is next touched.
- 23 rows are `optional`: dsh ships them off by default or experimental (agent teams, browser
  and computer use, schedule, programmatic tool calling, voice, hook bridges and others the
  survey marks so); they are outside the floor and out of the ranking. Some of them are planned
  seams all the same (programmatic tool calling, hooks, scheduled runs): optional in dsh,
  designed in p1.
- 44 rows are `different-by-design`; each cites its decision. Six of them carry the owner's
  batch authorisation of 2026-10-10 (`.env` layering, runtime invariants registry, plug-in
  inventory field on requests, plug-in timers, a host-side key-value store, live reload of a
  browser plug-in bundle); thirteen cite the owner's scope answers (telemetry, feedback and
  upload: never; DeepSeek account sign-in and billing: the direct API route only). No row
  carries an open owner question.
- 37 rows are planned seams (Decision 9), designed in the catalog's "Planned seams" section and
  in the nine unassigned issues #709 to #717 plus the design comments on #695 and #696.
- `scripts/adr.py check` requires the README index to be regenerated (`scripts/adr.py index`)
  in the pull request that lands this record.

## Alternatives considered

- **Keep `seams.md` as prose and the "split it later" rule.** Rejected by the owner on #129:
  the later split is the expensive rewrite.
- **Copy dsh's plug-in kernel** (a Cordis-style context with services, rows and scopes).
  Rejected by ADR-0071 and `AGENTS.md`: no registry, locator or auto-registration; p1 reaches
  the same seams with explicit composition, as the catalog's registry table shows.
- **Judge the floor by mirroring dsh's package graph.** Rejected: p1 is allowed to be leaner;
  the floor is per capability row, judged by which variation points stay protected, not by
  package count.
- **Make the floor feature parity ("never weaker than dsh" in every capability).** Rejected by
  the owner on 2026-10-10 ("probably too hard a phrasing"): the floor is modularity during the
  foundation phase; features are gaps to schedule, not violations.
- **Protect every variation point, speculative ones included.** Rejected: Larman's rule
  protects known points; an interface nobody calls for is the speculative abstraction
  `AGENTS.md` forbids.
- **Move every host-side port into `p1-contracts` now.** Deferred per row: `p1-contracts` stays
  small and cohesive, and a port moves when its second adapter is known (as ADR-0152 did for the
  front end).
- **Turn the survey's ranked gaps into a roadmap inside this record.** Rejected: scheduling is
  the lead's; the record fixes the rules and the catalog, not the order of work.

## Evidence

- Owner words: issue #129 review, 2026-10-10 ("Why would anyone use us?!"); issue #670,
  comment of 2026-10-10 01:5x (the four rules, D6 to D8, the dsh-floor children #690 to #696,
  the front-end port #697); question dialogs of 2026-10-10 ~13:3x, recorded in
  `~/.agents/xo/dispatch/p1-lead-20261004/dsh-survey/DRAFT-NOTES.md` ("Owner decisions
  2026-10-10 ~13:3x": floor scope and meaning, the three rules accepted, the lead batch,
  pillars.md stays a draft).
- The survey: `~/.agents/xo/dispatch/p1-lead-20261004/dsh-survey/SURVEY.md` (2026-10-10,
  227 rows; status counts above, recounted by script after the 2026-10-10 rework; which dsh
  packages are off by default or experimental comes from the survey's rows and findings),
  `family-1-*.md` to `family-16-*.md` (per-row evidence by
  dsh source path and line, dsh README section, p1 path and line, issue number) and `RUN.md`
  (method: one mapper and one independent verifier per family; family 14 by a smaller model;
  one status cell outside the vocabulary, normalised in the catalog to
  `different-by-design (ADR-0053)`). dsh source at `dsh-v0.2.0-rc.2`, commit 639ed01; p1 at
  b291a70a (origin/main 54b7775e at survey time, ab7c01ff when this record was drafted).
- p1 facts re-checked while drafting (2026-10-10, worktree at ab7c01ff): the crate list under
  `crates/`, the module list under `modules/`, the public traits of `crates/p1-contracts/src`
  (`Provider`, `Tool`, `ContextPolicy`, `AuthorizationPolicy`, `EventSink`, `CommitSink`,
  `Clock`), the ADR titles cited above, issues #690 to #697 and the last comment on #670 (read
  only), and ADR-0151's front matter in the #129 worktree.
- Owner scope answers of 2026-10-10 (afternoon): recorded in the same `DRAFT-NOTES.md` under
  "Owner scope answers 2026-10-10" and in the lead's brief `BRIEF-plan-fable-2.md` in that
  directory. The seam designs rest on the survey's family files and on dsh's source at the
  surveyed tag (the named packages' `README.md` and `src/index.ts`, read read-only on
  2026-10-10); each design names what stayed unknown.
- Acceptance when this lands: `python3 scripts/adr.py check` passes; `docs/design/seams.md`
  holds one table per family with 227 rows and a "Planned seams" section with eleven entries;
  issues #709 to #717 exist, unassigned; `AGENTS.md` carries the amended rules.
