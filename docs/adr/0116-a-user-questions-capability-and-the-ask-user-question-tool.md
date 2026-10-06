---
adr: 116
title: A user-questions capability and the ask_user_question tool
status: proposed
date: 2026-10-05
deciders: lead
supersedes: []
superseded_by: []
sources: [ADR-0001, ADR-0109, ADR-0115, issue #513, audit slice F, DECISIONS D27 D30 D31]
---
# ADR-0116: A user-questions capability and the ask_user_question tool

## Context

An agent that meets a genuine ambiguity can only guess or end its turn with a question in prose.
Issue #513 (tools slice F of the iris-tools audit, `~/.agents/xo/dispatch/p1-iris-tools-audit/AUDIT.md`
sections 1.14, 4.2 and 5.F) asks for `ask_user_question {questions: Q[1..4]}` with
`Q = {question, header (1..12), options: O[2..4], multi_select?}` and
`O = {label, description, preview?}`, answered by the user through the host, never by the model.
The donor (`iris-agent/src/tools/ask_user_question.rs:56`) accepts hidden answer fields in the
tool's own input that the host fills in; in p1 the model writes a tool's input, so that shape
would let a model fabricate answers.

p1 already asks the user one thing at run time: the authorization prompt. `AskBridge`
(`crates/p1-host/src/policy.rs`) races the turn's cancellation, denies at once when headless, and
hands the question to the front end's `Asker` (`LineAsker` on stderr/stdin, the TUI's permission
view), which returns `None` when no operator can answer any more. The owner ordered tools E–J
after #575 (D27, D30) on GPT-6.1 Sol high workers (D31).

## Decision

- **A new interface, `user-questions`**, in a new `modules/wit/interaction.wit`, allocated to the
  `tool` class only (`modules/capabilities.toml`) and granted only to `p1/ask-user-question`.
  Additive as in ADR-0109 and ADR-0115: the package stays `p1:module@1.0.0`.

  ```wit
  interface user-questions {
      record question-option { label: string, description: string, preview: option<string> }
      record question { question: string, header: string, options: list<question-option>,
                        multi-select: bool }
      record answer {
          /// Labels of the options the user chose, in option order.
          chosen: list<string>,
          /// What the user typed instead of, or beside, an option.
          free-text: option<string>,
      }
      variant asked {
          /// One answer per question, in question order.
          answered(list<answer>),
          /// The user dismissed the questions, or the turn was cancelled.
          cancelled,
          /// No user can answer: a headless run, or the front end is gone.
          no-interactive-user,
      }
      variant question-error { invalid(string) }
      ask: func(questions: list<question>) -> result<asked, question-error>;
  }
  ```

  The interface has no way to pass an answer in: answers come only from the host's front end.
- **The host validates every set** before showing it, whatever the guest checked: 1–4
  questions, unique question text; header 1–12 characters; 2–4 options with unique non-empty
  labels and non-empty descriptions; no option labelled `Other` (the front end supplies free
  text); no `preview` on a multi-select question; each string bounded (question and description
  2,000 bytes, preview 8,000 bytes). A violation is `invalid` with the first reason, and nothing
  is shown.
- **The host asks through the front end, as for authorization.** A `QuestionAsker` beside
  `Asker` in `p1-host`: the line front end prints numbered options on stderr and reads the
  choice or free text from the next line; the TUI shows a question view in the permission
  view's place; the headless host returns `no-interactive-user` at once. The ask races the
  turn's cancellation (`cancelled`); there is no timeout, so silence is never an answer. One
  question set is shown at a time per front end; a second, from a delegated worker, waits its
  turn. An authorization prompt and a question set never overlap. Delegated workers use the
  same service, labelled with the worker id.
- **The `p1/ask-user-question` tool** (`p1-tool-question` guest) maps the schema to the WIT
  records, rejects unknown fields, and formats the outcome: per question its header, the chosen
  labels and any free text; or `cancelled — no answer` or `no interactive user — decide without
  asking, or end the turn with the question`. The result passes the normal output bound. Its
  effect class is read-only: it changes nothing, so no authorization prompt precedes it.
- **Journal**: the question set and the outcome are the tool call and its result, journaled and
  redacted like every other; the asker adds no second record.

## Consequences

- An agent can ask the user up to four bounded multiple-choice questions and act on the answer;
  a model cannot write an answer, and cancellation or a headless run never reads as one.
- One more interface, one more component, one more front-end view; the allocation table and
  the boundary check change, and only `p1/ask-user-question` gains the capability.
- A headless run gets a plain refusal instead of a hung tool.
- Whether models ask when they should is unmeasured until the issue's five scripted ambiguity
  tasks run.

## Alternatives considered

- **Donor shape, answers in the tool input filled by the host**: the model writes that input,
  so an answer field is forgeable.
- **The question as an authorization prompt**: the authorization path decides permit/deny for a
  call; folding questions into it would widen `AuthorizationPolicy` and give a policy module
  user answers it should not see.
- **A timeout that picks the first option**: fabricates an answer from silence.

## Evidence

- Donor schema and hidden answer fields: `~/projects/iris-agent/src/tools/ask_user_question.rs:11`,
  `:56`, `:96`, `:142`.
- Asking pattern reused: `crates/p1-host/src/policy.rs` (`Asker`, `AskBridge`, `LineAsker`).
- Additive boundary precedent: ADR-0109, `docs/design/modules/wit.md` "Amendments after the
  freeze"; ADR-0115.
- To re-check after implementation: schema/runtime bound tests, the no-injection test, the TUI
  answer test, the headless refusal test, and the measured questions-asked-vs-needed counts on
  five scripted tasks.
