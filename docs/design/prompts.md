# Prompt profiles and fragments — specification

The system prompt is composed, not stored: a **profile** is an ordered list of **fragments**,
each fragment is one concern of the prompt, and the assembler picks for every fragment the
most specific wording it has for the agent's model family and model, drops fragments whose
tools the agent lacks, joins the rest, and then runs the placeholder engine `assembly.md`
already specifies. This document is the contract for that composition and the home of the V1
prompt text. It replaces the "`prompt.md` is a WHOLE prompt file per family … No fragment
engine" paragraph of `assembly.md` § Environment files; the ADR that lands with the code
supersedes ADR-0003 accordingly (§ Decision record).

Status: the V1 fragments ship under `environments/prompts/fragments/` with this document;
the assembler, the profile files and the migration of the shipped environments are issue
#779, tool-description variants are issue #780. Until #779 lands nothing under
`environments/prompts/` is read at run time.

Seam rows: "System prompt assembly" and "Per-agent persona" in `seams.md` (#779 updates
them; the `{{model}}` gap named there closes).

## Why fragments

p1's architecture rule (ADR-0153) is that a known variation point gets its port and its
adapters in the first slice. The prompt varies along three axes today: the **profile** (what
kind of agent: coder, Fable coder, manager), the **model family** (Claude and GPT want
different communication and tool-mechanics wording; the October 2026 Amp prompts differ
exactly there), and the **tool grant** (a worker with a subset of tools must not read about
tools it lacks). Fourteen whole prompt files cannot express that without drift: the shipped
`glm`, `glm-go` and `glm-messages` prompts are three identical copies kept in step by hand,
and the GPT prompt repeats the Claude prompt with eleven lines changed. One fragment per
concern, one file per family or model variant, one list per profile removes the copies and
makes every difference between two prompts a named file.

## Layout

```
environments/prompts/
  main.toml                      # profile: the coding agent
  fable.toml                     # profile: the coding agent for Claude Fable
  manager.toml                   # profile: the session manager
  fragments/
    <id>.md                      # base text of one concern
    <id>.<family>.md             # family variant (gpt, claude, glm, deepseek, kimi, …)
    <id>.<model-profile>.md      # model variant (claude-fable-5, gpt-5.6-sol, …)
```

Both roots that `assembly.md` already defines apply: the shipped tree and
`$P1_CONFIG_DIR/environments/prompts/` (default `~/.config/p1`). A user file shadows the
shipped file **of the same relative path**; a user profile shadows a shipped profile of the
same name. There is no merging of lists: a user `main.toml` replaces the shipped one whole.

`<family>` is the `family` key of the model profile in `profiles/`; `<model-profile>` is the
environment's `profile` key (the file name under `profiles/`, for example `claude-fable-5`,
not a provider's wire id). A suffix that is neither a known family nor a profile id is a
test failure (§ Tests), so a typo cannot silently create a dead variant.

## Profile file

```toml
description = "one line, printed by p1 env show"

fragments = [
  { id = "identity" },
  { id = "tool-files", requires_any = ["read", "edit", "write"] },
  { id = "delegation", requires = ["worker_start"] },
]
```

- `fragments` is ordered: prompt order is list order. Ids are `[a-z0-9-]+`, unique in a
  profile.
- `requires = [modules…]`: the fragment is kept only when **every** named tool module is
  assembled for this agent. `requires_any = [modules…]`: when **at least one** is. An entry
  may carry both; an entry with neither is always kept. The module names are environment
  module keys (`read`, `shell`, `worker_start`, …), the same names `{{#tool:…}}` uses.
- Unknown keys are errors (`deny_unknown_fields`), as everywhere in `environment.toml`.

A gate decides whether a whole fragment is present; `{{#tool:m}}…{{/tool:m}}` sections inside
a fragment still decide sentence by sentence (a `tool-files` fragment kept because `read` is
present still drops its `edit` sentence for an agent without `edit`). Use the gate for a
fragment that is meaningless without the tool, the section for a cross-reference.

## Resolution and composition

For each listed fragment, in order:

1. gate: drop the entry if its `requires` or `requires_any` fails;
2. resolve: the first existing file of `<id>.<model-profile>.md`, `<id>.<family>.md`,
   `<id>.md`, looked up in the user root and then the shipped root for each name in turn (so
   a user `identity.md` does not beat a shipped `identity.claude-fable-5.md`: specificity
   first, then root);
3. read it as UTF-8, strip leading and trailing blank lines.

Join the kept texts with one blank line (`"\n\n"`) and add a final newline. The result is the
environment's **prompt template**, the same artefact a `prompt.md` used to be; from here on
`assembly.md` applies unchanged: `render_prompt(template, modules, substitutions)` expands
`{{workspace}}`, `{{date}}`, `{{os}}`, `{{scratch}}`, `{{tool_names}}`, `{{tool:<module>}}`
and the `{{#tool:…}}` sections, and the host appends instruction files and the skill listing
(ADR-0151). Two placeholders are new: `{{model}}` (the environment's `profile` id) and
`{{family}}` (its family); `Substitutions` gains the two fields.

Composition is pure: `compose_prompt(profile, family, model_profile, modules, roots) ->
Result<Composed, AssemblyError>` reads only the files it names and returns the template plus
the provenance `p1 env show` prints (§ Observability). No fragment is ever registered,
discovered or auto-included: a fragment file nobody lists is dead, and a test says so.

## Selecting a profile

`environment.toml` gains one key: `prompt = "main"`. Exactly one of `prompt.md` and `prompt`
per environment; both or neither is an `AssemblyError` naming the environment. Aliases
(`alias_of`) inherit the target's choice. A worker started with an explicit `system_prompt`
still gets that text in full. An `inherit_prompt` subagent (ADR-0150) inherits the parent's
composed template, before substitution, exactly as it inherited `prompt.md`.

Errors, each its own `AssemblyError` variant with the file and key in the message: unknown
profile name; missing fragment (the message lists the three file names tried and both
roots); duplicate id; empty `fragments`; a gate naming a module key the catalog does not
know; unknown key in the profile file.

## Observability

`p1 env show <environment>` prints, before the rendered prompt it prints today: the profile
name and description, then one line per listed fragment: `kept  <id> -> <resolved file>` or
`dropped <id> (requires shell)`, and the SHA-256 of the composed template. The journal's
`AssemblyIdentity` gains `prompt_profile`, `prompt_fragments` (the resolved file names, in
order) and `prompt_template_sha256`, default-empty for old journals as ADR-0151 did for
instructions and skills. A prompt change is therefore visible in every run record as a
different hash and, when a variant was added, a different file name.

## Tool descriptions (#780)

The model-facing description of a tool is the Rust constant in its crate, written in the
Claude register (`tools.md` § Common rules), and an environment can replace it only with
`description_file` on a `[[tools]]` entry. #780 gives descriptions the same resolution:
`environments/prompts/tools/<module>.md`, `<module>.<family>.md`,
`<module>.<model-profile>.md`. A resolved file becomes the `ToolFace.description` override;
an explicit `description_file` on the entry wins over it; with no file the constant stays.
`p1 env show` names the source per tool and the journal records the description's hash. The
prompt's per-tool **guidance** (when to use which tool, mechanics the schema cannot say)
stays in the prompt fragments `tool-*`; the **description** stays with the tool. Together
they are the complete per-family tool story; neither duplicates the other.

## The three profiles

The lists below are normative; #779 ships them as the files named.

### `main` — the coding agent

```toml
description = "Coding agent: one task end to end in the user's workspace"

fragments = [
  { id = "identity" },
  { id = "workspace" },
  { id = "ownership" },
  { id = "scope" },
  { id = "conventions" },
  { id = "discovery" },
  { id = "verification" },
  { id = "care" },
  { id = "guidance" },
  { id = "tool-use" },
  { id = "tool-files", requires_any = ["read", "edit", "write"] },
  { id = "tool-apply-patch", requires = ["apply_patch"] },
  { id = "tool-grep", requires = ["grep"] },
  { id = "tool-shell", requires = ["shell"] },
  { id = "tool-todo", requires = ["todo_write"] },
  { id = "tool-question", requires = ["ask_user_question"] },
  { id = "delegation", requires = ["worker_start"] },
  { id = "workflows", requires = ["workflow_start"] },
  { id = "communication" },
  { id = "finish", requires = ["finish"] },
]
```

### `fable` — the coding agent for Claude Fable

Same concerns as `main`, with `discovery` moved before `scope` and the `principles` fragment
added after `conventions`; the Fable wording of `identity`, `ownership`, `discovery`, `care`
and `communication` comes from their `.claude-fable-5.md` variants, so an environment that
runs `main` on Fable also gets it. Running `fable` on another model gives that model the
base wording plus the principles.

```toml
description = "Coding agent for Claude Fable: main plus engineering principles"

fragments = [
  { id = "identity" },
  { id = "workspace" },
  { id = "ownership" },
  { id = "discovery" },
  { id = "scope" },
  { id = "conventions" },
  { id = "principles" },
  { id = "verification" },
  { id = "care" },
  { id = "guidance" },
  { id = "tool-use" },
  { id = "tool-files", requires_any = ["read", "edit", "write"] },
  { id = "tool-apply-patch", requires = ["apply_patch"] },
  { id = "tool-grep", requires = ["grep"] },
  { id = "tool-shell", requires = ["shell"] },
  { id = "tool-todo", requires = ["todo_write"] },
  { id = "tool-question", requires = ["ask_user_question"] },
  { id = "delegation", requires = ["worker_start"] },
  { id = "workflows", requires = ["workflow_start"] },
  { id = "communication" },
  { id = "finish", requires = ["finish"] },
]
```

### `manager` — the session manager

The role Puck plays for Amp: it inspects, dispatches, steers and reports, and does not code.
Project policy (how work moves in a repository: claiming, brief rules, landing) is **not** in
the prompt; it arrives through the appended `AGENTS.md` chain of the workspace the manager
runs in, so the same prompt manages any repository.

```toml
description = "Session manager: dispatches, steers and reports on coding workers"

fragments = [
  { id = "manager-role" },
  { id = "workspace" },
  { id = "manager-truth" },
  { id = "guidance" },
  { id = "tool-use" },
  { id = "tool-files", requires_any = ["read", "edit", "write"] },
  { id = "tool-grep", requires = ["grep"] },
  { id = "tool-shell", requires = ["shell"] },
  { id = "tool-github", requires_any = ["read_github", "list_directory_github", "glob_github", "search_github", "commit_search", "diff_github", "list_repositories"] },
  { id = "manager-delegation", requires = ["worker_start"] },
  { id = "manager-shipping" },
  { id = "manager-communication" },
  { id = "finish", requires = ["finish"] },
]
```

**Correction, owner 2026-10-11: the manager is a pane and session manager, as Puck is, not
an orchestrator.** Puck's workers are peer sessions the user follows in their own panes;
after a dispatch it acknowledges and stops, it asks for a reply only when a result is
needed, and it has no shell and no filesystem. The V1 profile above got the role,
truthfulness, shipping and communication right, but `manager-delegation` was written
against p1's child workers (`worker_start` and its siblings), which run inside the
manager's session and report back to it, and so it reads as an in-session dispatcher
("delegation is your main instrument", the unattended rule to block on `worker_result`).
That fragment is replaced by `manager-sessions`, gated on the session-manager tools of
#833 (observe, start, message and wait for peer sessions, plus GitHub issues and CI
status), in #834; the manager then holds no shell, read or grep. The profile ships with
#779 as data only, so that every `manager-*` fragment stays listed; no manager environment
lands before #834.

The `manager` environment (`environments/manager/environment.toml`, #834): route
`openai-codex-subscription`, profile `gpt-5.6-sol` with reasoning off (`[options.native]
"openai-responses.reasoning_enabled" = false`, as `librarian`; ADR-0129), `prompt =
"manager"`, tools: the eight session tools and the ten GitHub tools of #833, `finish`,
`skill`, `ask_user_question`; `[capabilities] workers = false`, so the host appends no
child-worker tools. Owner decision 2026-10-11: GPT-5.6 Sol with reasoning off, as Puck,
and an Anthropic model with reasoning off as the fallback.

The fallback is a second environment, `manager-claude`: the same tools and `prompt =
"manager"` on route `anthropic-subscription`, profile `claude-sonnet-4-6` with no
`reasoning_effort`. The Messages adapter refuses an explicit `off` (ADR-0170 §3), and an
effort-less request on a `budget` profile sends no thinking field at all
(`crates/p1-provider-anthropic/src/request.rs`, `lower`), which for the 4.6 models means no
extended thinking; the Claude 5 profiles think by the server's default and p1 has no way to
turn them off. ASSUMPTION: Sonnet 4.6 rather than Opus 4.6, the only other such profile. A session
moves to the fallback by hand (`--model manager-claude/claude-sonnet-4-6`, or `/model`)
until the session-level fallback chain lands (#820).

The manager starts coding work as peer sessions in the `claude` environment through the
session tools, so each worker gets the full coding prompt and its own pane; there is no
`coder` subagent entry and no child worker. The `manager-*` fragments have no family
variants; a GPT manager and a Claude manager differ only in the base fragments' variants.

## Fragment inventory (V1)

| id | concern | gate | variants | source |
|---|---|---|---|---|
| `identity` | who the agent is, workspace line | — | `claude-fable-5` | Amp Opus `identity`; Fable §1 opening |
| `workspace` | scratch dir, stay inside the repository, no installs, injected text is evidence | — | — | p1 `claude/prompt.md`; HIGH `guidance_loading` |
| `ownership` | complete the request, intent, decisions, persistence, others' changes, continuity | — | `claude-fable-5` ("Role and intent calibration") | Opus `autonomy_and_persistence`; HIGH `task_ownership`; Fable §1 |
| `scope` | smallest change, no unrelated cleanup, files, abstractions, one hat | — | — | Opus `pragmatism_and_scope`, `engineering_judgment` |
| `conventions` | repository as style guide, closest analog, new pattern deliberately | — | — | Fable §3 (same text in Opus `discovery` 75) |
| `discovery` | read until ownership is clear, claims, contradicting evidence, hypotheses | — | `claude-fable-5` ("Investigate before acting") | Opus `discovery_discipline`; HIGH `discovery`; Fable §2 |
| `principles` | engineering defaults | — (fable only) | — | Fable §4 |
| `verification` | scale to risk, faithful reporting, validation vs kept tests, test design, UI | — | — | Opus `verification`; HIGH `verification`, `test_design` |
| `care` | free vs ask, destructive shortcuts, commit/push, scoped authorization | — | `claude-fable-5` ("Executing actions with care") | Opus `actions_requiring_explicit_approval`; HIGH `authorization`; glm prompt "Protect existing work"; Fable §6 |
| `guidance` | AGENTS.md scoping, skills, no standing-instruction drift | — | — | Opus `guidance_files`, supplement `skills`; HIGH `guidance_loading` |
| `tool-use` | tool list, context first, parallel calls, failures, secrets | — | — | Opus `tool_use`; HIGH `tool_operation`; Fable §7, §10 |
| `tool-files` | read/edit/write mechanics | any of read, edit, write | — | p1 `claude/prompt.md`; `tools.md`; Fable follow-up "Editing mechanics" |
| `tool-apply-patch` | the V4A patch format | apply_patch | — | p1 `gpt/prompt.md` |
| `tool-grep` | modes, literal, scope, paging | grep | — | `tools.md` § grep |
| `tool-shell` | no terminal, timeout, background, stored output, commit messages | shell | `gpt` (adds rg and ranged reads) | p1 prompts; `tools.md` § shell; Opus `tool_use`, `shell_details` |
| `tool-todo` | the to-do list | todo_write | — | tool constant |
| `tool-question` | invitation-only questions (ADR-0135) | ask_user_question | — | Opus supplement `questions_and_prompts` |
| `tool-github` | the read-only GitHub tools | any GitHub tool | — | `github-tools.md`; Puck §9 |
| `delegation` | do it yourself by default, when to delegate, briefs, verify results | worker_start | — | Opus `delegation`; HIGH `delegation`, `worker_briefs`; Fable §8; p1 Workers section |
| `workflows` | Rhai workflows, only when asked | workflow_start | — | p1 `claude/prompt.md` (unchanged text) |
| `communication` | progress, final answer, delivery state, prose, diagrams | — | `gpt`, `claude-fable-5` | Opus `communication`; HIGH `progress_communication`, `completion_and_delivery`, `writing_style`; Fable §9 |
| `finish` | the finish contract | finish | — | p1 prompts (the pinned sentence verbatim) |
| `manager-role` | who the manager is | — | — | Puck §1 |
| `manager-truth` | never invent state, done means merged | — | — | Puck §3, §8 |
| `manager-delegation` | new vs continue, briefs, interrupts, idle, decisions; V1 text against child workers, replaced by `manager-sessions` on the session tools in #834 | worker_start | — | Puck §4, §6, §7 |
| `manager-shipping` | merge authorization, own shell, red main | — | — | Puck §8, §12 |
| `manager-communication` | register, state first, when to write | — | — | Puck §2, §7 |

"Opus" is Amp's October 2026 medium-mode prompt on Opus 5.5, "HIGH" the GPT-6 high-mode
prompt, "Fable" the ultra-mode prompt, "Puck" Amp's dispatcher; all as self-reported and
collected in the owner's prompt-lab. Where Opus and HIGH say the same thing in different
words the base takes Opus's sentence (the shorter) and the `.gpt.md` variant exists only
where the GPT prompt differs in substance: the opening progress message and reading files
through the shell.

## Authoring rules

1. One concern per fragment; a variant replaces the whole fragment and keeps its claims
   unless there is evidence that the family needs different ones. A new rule for every model
   goes in the base; a rule one model needs goes in its variant; a new concern is a new
   fragment and a profile entry.
2. Tools are named only as `{{tool:<module>}}` inside `{{#tool:<module>}}…{{/tool:<module>}}`
   or in a fragment whose entry `requires` the module; never as a literal name in backticks.
   Parameters are named as the schema spells them (`timeout_seconds`, `replace_all`).
3. Headings are Markdown `#` lines; a fragment without a heading continues the previous
   section (the `tool-*` fragments continue `# Tools`). Pinned literals: `# Workers
   (optional)`, `# Finishing`, and the sentence beginning `Every task ends with a` verbatim.
4. Register: the Anthropic prompting style, dense. One rule per bullet, concrete nouns,
   active verbs, no filler, no emphasis markup, examples only where a rule is ambiguous
   without one (the patch format, the Rhai script). Say what to do and the reason when the
   reason changes behaviour; do not explain the obvious.
5. Behaviour that holds in every workspace goes in the prompt; behaviour of one repository
   goes in its `AGENTS.md`; tool mechanics the schema already states go nowhere.
6. Every fragment change: render each profile that lists it (`p1 env show` after #779) and
   read the result as the model will; check the coherence test's subset sweep still passes.

## Migration (#779)

| environment | model profile (family) | after | notes |
|---|---|---|---|
| `claude`, alias `claude2` | claude-sonnet-5 (claude) | `prompt = "main"` | base wording |
| `gpt` | gpt-5.6-sol (gpt) | `prompt = "main"` | `tool-shell.gpt.md`, `communication.gpt.md` |
| `zen`, aliases `zen2`, `zen3` | space-bunny-free (space-bunny) | `prompt = "main"` | persona line as `identity.space-bunny-free.md`; instructions stay disabled |
| `kimi` | kimi-k3 (kimi) | `prompt = "main"` | persona as `identity.kimi-k3.md`; each rule of the tuned prompt the base lacks becomes a `.kimi.md` variant or is dropped, listed in the PR |
| `glm`, `glm-go`, `glm-messages` | glm-5.3 (glm) | `prompt = "main"` | three copies become one; "Protect existing work" is already in base `care`; remaining deltas as `.glm.md` variants or dropped, listed |
| `deepseek`, `deepseek-messages`, `cline`, aliases `deepseek1..3`, `cline2` | deepseek-v4.1-flash (deepseek) | `prompt = "main"` | as glm, `.deepseek.md` |
| `deepseek-review` | deepseek-v4.1-flash | keeps `prompt.md` | a reviewer profile is a later slice |
| `finder`, `librarian`, `task` | — | keep `prompt.md` | subagent prompts: later slice; `task` inherits the parent's composed template |
| `manager` (new, #834 after #833) | gpt-5.6-sol (gpt), reasoning off | `prompt = "manager"` | § The three profiles; the profile file ships with #779, the environment with #834 |
| `manager-claude` (new, #834) | claude-sonnet-4-6 (claude), no effort | `prompt = "manager"` | the manager's fallback, selected by hand until #820 |

Every migrated environment deletes its `prompt.md` in the same pull request. The
`lead_prompt_coherence` sweep must pass for each before and after; a behavioural rule that
the tuned prompts carried and the base lacks is a decision the implementing worker lists in
the pull request body (kept as variant, folded into base, or dropped with the reason), never
a silent loss.

## Tests

- `crates/p1-assembly/tests/prompt_sections.rs`: unchanged (the placeholder engine).
- `crates/p1-assembly/tests/lead_prompt_coherence.rs`: unchanged in intent; its `check` reads
  the composed template instead of `prompt.md` and still renders every subset containing
  `finish`, demands the placeholder mention of every assembled tool, forbids literal tool
  names, and pins `# Finishing`, `# Workers (optional)` and the answer-only sentence.
- New, in `p1-assembly`: resolution order (a model variant beats a family variant beats the
  base; user root beats shipped root at equal specificity); `requires` and `requires_any`;
  each error variant; every shipped profile renders for every shipped environment that names
  it, for every tool subset containing `finish`; every file under `fragments/` is listed by
  at least one profile; every variant suffix is a family of some `profiles/*.toml` or a
  profile id; `env show` output names the fragments and the hash.
- Fixtures: temp directories only; the shipped tree is read, never written.

## Decision record

The ADR that lands with #779 (number reserved on #766) is titled "The system prompt is
composed from fragments by profile, family and model". It supersedes ADR-0003 (`--supersedes
3`) and restates what still holds from it: the harness reshapes itself around the model,
the environment directory stays the unit of configuration, assembly fails before a run
starts, a prompt can never name a tool the agent lacks. It replaces the sentence "No
fragment engine" with this document's contract and records why: fourteen whole files had
started to drift, and the family and tool-grant axes are known variation points, which
ADR-0153 says get their port in the first slice. Consequences it records: `Substitutions`
grows two fields (a public interface change), `environment.toml` grows `prompt`, the journal
identity grows three default-empty fields.

## Assumptions and owner decisions

Parked in `~/.agents/xo/for-owner.md` on 2026-10-11; the owner answered the same day in the
session dialog. An open item proceeds on its first option.

1. OPEN. Fable gets both a profile (`fable`, adds `principles`) and model variants, so
   `main` on Fable also speaks Fable's wording. Alternative: variants only, one profile.
2. DECIDED 2026-10-11 (owner): the manager runs GPT-5.6 Sol with reasoning off, as Puck,
   and an Anthropic model with reasoning off as the fallback; applied in § `manager`. The
   first draft had Opus 5.5 on the Anthropic subscription. Remaining ASSUMPTION there: the
   fallback model is `claude-sonnet-4-6`, and the automatic move is #820.
3. DECIDED 2026-10-11 (owner): keep. Tool descriptions keep the Rust constants as baseline
   and add file variants (#780). Rejected alternative: move every description into
   `environments/prompts/tools/` and make the constant a fallback for tests only.
4. DECIDED 2026-10-11 (owner): keep. `scope` stays in the `fable` profile although Amp's
   Fable prompt has no pragmatism section; `scope` is restraint, `principles` is design
   quality, and this repository's `AGENTS.md` asks for the smallest design everywhere.
5. DECIDED 2026-10-11 (owner): keep. The cheap-model prompts (glm, kimi, deepseek) lose
   their tuned wording in favour of the base plus listed variants; their persona lines stay
   as model variants of `identity`, and the per-rule list in the #779 pull request is the
   safeguard against silent loss.
