---
adr: 177
title: Agents follow Amp's working behaviour; the fleet process rules are withdrawn
status: accepted
date: 2026-10-11
deciders: owner
supersedes: [128, 176]
superseded_by: []
sources: [AGENTS.md]
---
# ADR-0177: Agents follow Amp's working behaviour; the fleet process rules are withdrawn

## Context

p1 work now runs through Amp: Amp's own agents and Claude Code in Amp's Claude Code mode, mostly
in orbs that build on their own machines. The repository's `AGENTS.md` still carried the fleet's
process rules (a lead that dispatches, a mandatory review before the first push, at most three
issues at once, ten-line briefs, an owner-question file, label claims, Amp lanes and machine
limits), and several of them contradict how Amp's agents are instructed to work. On 2026-10-11 the
owner decided (voice message, fillers removed): "wherever our instructions contradict the Amp
code instructions, the Amp code instructions are the ones we want to follow", and "for most of
the implementation, we can put up to 20 or more together, work in parallel if it makes sense
coding-wise and they are not conflicting with each other."

## Decision

`AGENTS.md` keeps only project facts and hard rules: the landing commands and their standing
authorization, the gate, the pre-push check and its docs-only exemption (ADR-0169), worktree and
build hygiene, the architecture of ADR-0153, and the test and data rules. Everything about who
dispatches, reviews or briefs whom is withdrawn; agents work the way Amp instructs them.
1. Any number of issues run in parallel when their files do not overlap (replaces ADR-0128
   point 1 and ADR-0176).
2. No mandatory review before the push; a review happens when the owner asks or an agent needs a
   second opinion on a specific high-impact question (replaces ADR-0128 point 2).
3. Briefs, owner questions and issue labels follow Amp's behaviour, not a fixed format, file or
   claim protocol (replaces ADR-0128 points 8 and 9).
4. ADR-0128 points 4, 6 and 7 stand: the docs-only gate path, one `scripts/pre-push.sh` per pull
   request, SSD targets retired after the merge. Decision records stay required for a changed
   public interface or contract, a new dependency, or a reversed decision; a changed workflow rule
   no longer needs one.

## Consequences

Agents in parallel orbs stop waiting on a review stage, a count limit or a lead. Defects a
pre-push review would have caught reach the gate or main and are fixed afterwards. Local builds
stay bounded by `scripts/rustc-serial` and `scripts/build-admission.sh`, which queue builds instead
of limiting issues. ADR-0153's modularity rules stay in force even though Amp's default prefers
fewer abstractions: they are the product's architecture, and Amp's agents follow the module
structure a repository's `AGENTS.md` prescribes.

## Alternatives considered

Keeping the pre-push review as an Amp-compatible option: rejected, Amp's agents treat an
unrequested review as a gate they should not add. Keeping the three-issue limit for workstation
builds: rejected, the build wrapper already queues local compiles.

## Evidence

The Amp agents' own descriptions of their instructions, collected by the owner on 2026-10-10 and
2026-10-11 (local, untracked `prompt-lab/AMP-Code-*.md`). Pull request #776 (ADR-0176) had already
lifted the count limit for orbs.
