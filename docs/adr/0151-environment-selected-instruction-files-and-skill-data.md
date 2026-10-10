---
adr: 151
title: Environment-selected instruction files and skill data
status: accepted
date: 2026-10-10
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0151: Environment-selected instruction files and skill data

## Context

Issue #129: owner ordered instruction files and skills like Pi, Claude Code and
Codex, so p1 comparisons start with equivalent standing rules. The lead's
2026-10-10 brief settles the approximation below; these are lead design choices,
not claims that p1 exactly reproduces each vendor's harness.

Lead research (S = source verified, D = documented):

| Harness | Files | Walk | Cap | Placement | Skill body |
|---|---|---|---|---|---|
| Codex | AGENTS.override.md > AGENTS.md (S) | git root → cwd (S) | 32768 B, truncates (S) | user (S) | read listed path (S) |
| Kimi Code | AGENTS.md (S) | git root → cwd (S) | warns above 32 KB (S) | system (S) | Skill tool (S) |
| ZCode | AGENTS.md (S) | nearest only (S) | 100 KiB/file (S) | user (S) | Skill tool (S) |
| Claude Code | CLAUDE.md, else AGENTS.md (D) | filesystem root → cwd (D) | none (D) | user (D) | Skill tool (D) |
| Pi | AGENTS.md > CLAUDE.md (S) | filesystem root → cwd (S) | none (S) | system (S) | read path (S) |
| dsh | AGENTS.md + CLAUDE.md (D) | git root → cwd (D) | 65536 B (D) | user (D) | skill tool (D) |

## Decision

Load prompt data once at assembly, including child-agent assembly. Append an
`instructions` XML section after rendered environment text, then a `skills`
section only when the selected tools include module `skill`. No template edits.
`p1-assembly` owns instruction loading and settings. Owner decision 2026-10-10
"Skill home": dsh-like separation. `p1-contracts` holds skill summary/loaded
data and the list/load source interface, without file I/O or YAML. `p1-skill-fs`
owns discovery and frozen bodies. `p1-tool-skill` owns the model's listing and
read-only, Shared tool, depending only on the source contract (never the disk
source). Host joins them through assembly's source factory/listing callback.
Append `skill` after each environment's existing tool entries, preserving their
order and declaration prefix (lead correction 2026-10-10).
The core stays unchanged. Both disk readers share the existing protected-open
checks extracted into `p1-workspace`, not duplicate credential logic.

Owner decision 2026-10-10 "YAML reader": own small two-field reader in
`p1-skill-fs`, no YAML dependency. Accept plain, single-quoted, double-quoted
(JSON/common escapes), folded `>`/`>-` and literal `|`/`|-` values for name and
description; ignore other fields and nested maps under them. This is deliberately
not general YAML: unsupported field forms warn and skip that skill.

Optional `[instructions]`: `enabled = true`, `files = ["AGENTS.md"]`,
`max_bytes = 32768` (1..1048576). Read global `instructions_global` from
settings.toml (default `~/.agents/AGENTS.md`), then first existing filename per
directory from nearest git root to workspace, workspace alone without git.
Relative global overrides resolve against workspace. Missing global is silent;
other unreadable inputs warn and skip. Truncate crossing file at UTF-8 boundary,
drop later files, emit one visible notice naming affected paths.

Optional `[skills]`: `roots = ["~/.agents/skills", ".agents/skills"]`,
`max_listing_chars = 8000` (100..100000). User roots precede project roots;
preserve configured order within groups and sort directory entries. Discover
one-level SKILL.md files, parse YAML name/required description, first name wins.
Snapshot bodies with metadata at assembly; `skill {name}` returns that body
without front matter, its absolute directory, and at most 100 KiB of body with
a notice. Protected regular-file reads are bounded at 1 MiB during discovery.
Descriptions are at most 1024 characters; names are 1–64 lowercase ASCII
letters/digits/hyphens. Invalid skills warn and skip. Listing metadata is XML
escaped. Over listing budget, retain all names without descriptions and say so;
names-only fallback is not an omission budget.

| p1 environment | Filename preference | Skill roots / tool |
|---|---|---|
| claude | CLAUDE.md, AGENTS.md | claude and agents roots, user before project; skill |
| gpt | AGENTS.override.md, AGENTS.md | default roots; skill |
| glm, glm-messages, kimi, deepseek, deepseek-messages | AGENTS.md | default roots; skill |
| zen | disabled | no skill |
| other bases | defaults | no skill |
| aliases | inherit base | inherit base; no duplicate blocks |

Record exact loaded-text SHA-256 and instruction paths/sizes plus skill
names/paths in serde-default fields on existing journal AssemblyIdentity.
The Environment record already records prompt text. `env show` prints effective
settings, loaded paths/sizes, loaded instruction hashes and skill body hashes.

## Consequences

Assembly is deterministic for identical inputs; tool output and provenance refer
to the same frozen data. New readers load old journals with empty provenance;
older binaries can reject populated new fields because identity records deny
unknown fields. No credential contents enter prompt data: reuse protected file
reads. Zen's disabled instructions/no-skill selection avoids automatic private
rules in free-tier prompts; explicit legacy CLI flags remain explicit overrides.
Existing opt-in `--instructions`/`--skills` behavior is unchanged and separate.

## Alternatives considered

- User-role placement deferred: interacts with compaction's first-user-message
  task rule; system placement is compatible with current assembly.
- Lazy subdirectory files deferred: needs file-visit state and dynamic injection.
- `@path` imports deferred: needs traversal/cycle and credential semantics.
- Mid-session reload and `/reload` deferred: require cache/journal identity rules.
- `.local.md` overlays deferred: need explicit precedence and privacy semantics.
- No executable skill registry, vendor wrapper text or provider-specific loader.

## Evidence

- `p1-assembly/tests/instructions_skills.rs`: order, first match, UTF-8 byte cap,
  missing global, no git, disabled selection, collision/validation/listing and
  shipped environment inventory including aliases, all over temporary roots.
- `p1-host/tests/instructions_skills.rs`: catalog tool execution, unknown name,
  body cap, settings override and actual assembly identity.
- `p1-journal/tests/journal_version.rs`: provenance round trip and old records.
- `p1-skill-fs/tests/front_matter.rs`: supported forms, ignored nested maps,
  CRLF, missing closing delimiter and invalid/empty descriptions via discovery.
- `p1 env show claude` and `zen` with fake HOME and a scratch workspace exercise
  the operator-facing path without real user data or model requests.
