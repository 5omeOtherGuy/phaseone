---
adr: 135
title: ask_user_question is user-invocable only
status: accepted
date: 2026-10-08
deciders: owner
supersedes: []
superseded_by: []
sources: [ADR-0116, issue #513, DECISIONS D33 D37]
---
# ADR-0135: ask_user_question is user-invocable only

## Context

ADR-0116 shipped `ask_user_question` in every environment for the model to use on its own
judgement. The row-5 measurement on #513 gave five scripted tasks, each with one ambiguity the
workspace cannot resolve: DeepSeek V4.1 Flash asked 0 of 5 needed questions (2026-10-06) and
Claude Sonnet 5 asked 0 of 5 (2026-10-08). A control run that told the model to ask got a
question, the answer and work that used it. The owner decided on 2026-10-08, in a question
dialog, that the tool stays in every environment but is user-invocable only: the model uses it
only when the user asked for questions in this session.

## Decision

The declaration stays in every tool list, and its description states the rule in one
sentence. The host refuses a call unless a user input of this session invited questions. The
refusal returns `ask_user_question is only available after the user asks you to ask questions;
decide yourself and continue` with status error, and nothing is shown to the user.

A user input invites questions when it contains one of these phrases
(`crates/p1-host/src/questions.rs` `INVITING_PHRASES`): `ask me`, `ask_user_question`,
`ask user question`, `ask questions`, `use the question tool`. The match ignores case and works
on whole words: every character other than a letter, a digit or `_` becomes a space first, so
`Ask-User-Question` matches and `flask messages` does not.

User inputs are the headless prompt, every interactive line sent to the model, and the TUI's
submitted prompts, steering and follow-ups. On `--resume`, the restored `User` items and
steering also count. Host commands (`/model`, `/help`, …) do not count, and neither does text
the host writes itself (continuation, retry and inbox notifications) or a delegated worker's
task. The invitation lasts for the rest of the process and is shared by every worker, because
workers ask through the parent's front end. A headless run without an interactive user still
answers `no interactive user`, as before.

The rule lives in `QuestionBridge`, the host's user-questions service, which returns the new
`Asked::NotInvited`. The runtime sends that to the component as the existing
`question-error::invalid(string)` carrying the refusal text, and the guest shows that text
verbatim instead of its `Invalid input` prefix. The WIT interface, the module protocol and
every other component's digest stay unchanged; only `p1/ask-user-question` is rebuilt, for the
new description and the verbatim refusal.

## Consequences

- A model that asks without an invitation gets one error line and continues on its own
  judgement. The tool list does not change mid-session, so the cached prompt prefix holds.
- The match is deterministic but literal. A negation such as "don't ask questions" still
  invites, and a request in other words ("check with me first") does not. Either way, the
  outcome is no worse than the behaviour before this decision.
- Changing the phrase list changes this contract. The list is part of this record; extend it
  by amendment.
- The guest and the host each hold a copy of the refusal text.
  `crates/p1-tool-question/tests/component.rs` asserts that the two copies are equal.

## Alternatives considered

- **Add a `not-invited` case to the WIT `asked` variant.** This is the most explicit, but it
  changes the `p1:module` interface and so every tool world built from it. It would bring a
  protocol question and digest churn across all components, for one refusal string.
- **Deny the call in the authorization bridge, by tool name.** This needs no module change.
  But the policy layer judges effects, not tools. The rule would have to be wired into every
  front end's policy and every reload generation, and a custom front end could drop it.
- **Remove the declaration until invited.** The tool list would change mid-session, which
  rewrites the cached prompt prefix.
- **A prompt-only rule.** It is not enforced, and the row-5 runs show models do not follow
  usage guidance for this tool reliably.

## Evidence

- Issue #513 comments of 2026-10-06 (DeepSeek, 0 of 5) and 2026-10-08 (Claude Sonnet 5,
  0 of 5); run journals in `~/.agents/xo/dispatch/p1-lead-20261004/row5/`.
- Tests: `cargo test -p p1-host --lib question`
  (`only_the_documented_phrases_invite_questions`,
  `questions_are_refused_until_a_user_input_invites_them_and_workers_share_it`,
  `operator_text_for_the_model_invites_questions_and_a_slash_command_does_not`), and
  `cargo test -p p1-tool-question --test component an_uninvited_call_is_refused_with_the_host_text`.
  The component test needs `scripts/build-modules.sh --all` first.
