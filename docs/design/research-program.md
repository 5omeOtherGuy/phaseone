# Research program

How p1 research fans out without becoming shelfware (ADR-0045, issue #25). Development stays
with the lead; this document governs research only.

## 1. Rule

Every research item ends **used** or **discarded**. The unit of progress is a decision applied
and verified, or an idea explicitly closed — never a report produced.

## 2. States

A research item is a GitHub issue labelled `research` plus exactly one state label. GitHub is the
only state authority.

| State | Entered when | Who |
|---|---|---|
| `research:queued` | One falsifiable question, a named consumer artefact (path/field), baseline, maximum evidence effort; closed research issues were searched for duplicates. No consumer, no admission. | lead |
| `research:active` | A leaf is assigned with a pinned source revision, an output directory and a budget. | curator |
| `research:decision` | The curator checked the evidence and attached a decision memo (§4). | curator |
| `research:implement` | The lead accepted; a lead-owned spec/brief/development issue is linked. | lead |
| `research:used` | The change is merged and its effect verified — or the baseline is knowingly retained and the finding recorded where it is consumed. Issue closed. | lead |
| `research:discarded` | Reason, evidence and a precise reopening condition recorded, with keywords for model, route, symptom and mechanism. Issue closed. | lead |

"Deferred" is not a state. What nobody will schedule is discarded with a reopening condition.

**Caps:** 3 active, 2 in decision, 1 in implement, 6 admitted in total. While two memos wait,
nothing new is dispatched. Ideas may queue without being researched.

Open decisions: `gh issue list --label research:decision`.

## 3. Where things live

- Scratch and raw leaf output: `../phaseone-briefs/research/<issue>/` (not in the repository).
- The accepted, sanitised memo: `docs/research/<issue>-<slug>.md`, integrated by the lead.
- Development runs stay in `docs/dogfood/runs.jsonl`; an experiment keeps its own manifest and
  results next to its memo. Model cards receive only findings that change routing or prompting.
- Never raw journals, private prompts, credentials or authenticated traffic. Public sources are
  cited by URL, section and date; fixtures are synthetic and say so.

## 4. Decision memo

At most 600 words plus an evidence table:

1. **Decision requested** — accept this exact change, retain the baseline, or discard.
2. **Claim and scope** — model, route, task class, revisions; fact, observation or hypothesis.
3. **Evidence** — citations or run ids, contrary cases, what is missing.
4. **Confidence** — why it is or is not sufficient; no invented numbers.
5. **Proposed artefact** — exact path/field and the minimal text; interfaces unchanged.
6. **Verification** — metric, baseline, repetitions, acceptance rule, budget, rollback.
7. **Cost and ownership** — consumer, review burden, dependencies, invalidation trigger.

A memo whose decision requires the lead to re-read every source has failed curation.

## 5. Curator

A capable model engaged for **one batch**, not a standing supervisor. It may refine admitted
questions, dispatch read-only research leaves (DeepSeek V4.1 Flash on `opencode-go-2`, no
descendants), check sources and arithmetic, reject unready work, and hand the lead at most two
memos per batch. It may propose prompt text, configuration values and experiment manifests
*inside a memo*.

It never writes code, fixtures, prompts, profiles, routes, ADRs or `STATUS.md`; never dispatches
implementation or test-author workers; never commits, merges or accepts code; never reads
credentials, changes routes or spends on other accounts. Live provider calls for experiments:
zero, unless the lead wrote down a budget for that batch. A leaf that exits 0 with zero responses
or tokens has not completed.

## 6. Machine admission

| Class | Limit |
|---|---|
| Build-free, offline (reading, public docs, analysis, manifests) | up to 3 leaves |
| Build-free, live (pinned p1 binary, synthetic tasks, no cargo) | 1 run at a time, approved budget |
| Build-light (one small crate, filtered tests; Python tooling) | 1 beside the lead, if a lane is free |
| Build-heavy (workspace gate, host/contracts changes) | 1 machine-wide; no new cargo job while it runs |

`scripts/rustc-serial` stays at two slots; it is a backstop, not a scheduler. No new build work
below 1.5 GB available memory or 10 GB free disk. Research leaves run no Rust build.

## 7. Experiments on model behaviour

Scripted providers prove harness behaviour, never how a model responds. A prompt or setting
claim needs a matched design: one change, frozen harness/profile/route/tasks, fresh workspace and
session per run, randomised arm order, a predeclared primary outcome with its denominator,
independent judging, invalid runs reported, and confirmation on unseen tasks before a general
claim. Screening size: 4 tasks × 2 repeats × 2 arms. Not attempted here: model × prompt × effort
grids, model rankings from unrelated jobs, window tuning by brute force, latency claims from
scripted transports.

## 8. Review of the arrangement

After two batches the lead compares review minutes and clarification rounds against verified
improvements. If the curator only forwards reports, the layer is removed.
