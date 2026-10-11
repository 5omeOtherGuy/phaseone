# Prompt profiles and fragments

V1 of p1's system prompts, written as fragments that the assembler composes per profile,
family and model. The specification is `docs/design/prompts.md`; the assembler and the
migration of the shipped environments are tracked in the issue that document names. Until
that lands, every shipped environment still uses its whole `prompt.md`, and nothing in this
directory is read at run time.

## Layout

- `<profile>.toml` (lands with the assembler): the ordered fragment list of one profile, each
  entry `{ id, requires?, requires_any? }`. Profiles: `main` (the coding agent), `fable`
  (`main` plus engineering principles; the Fable wording comes from model variants), `manager`
  (the session manager that dispatches and steers workers).
- `fragments/<id>.md`: the base text of one concern, in prompt order a heading (`# …`) and
  bullets, or a heading-less group that continues the previous section.
- `fragments/<id>.<family>.md`, `fragments/<id>.<model-profile>.md`: variants of the same
  concern for one family (`gpt`, `claude`, …, the `family` of the model profile) or one model
  profile (`claude-fable-5`, `gpt-5.6-sol`, …, the `profile` key of the environment). The most
  specific file wins and replaces the whole fragment.

## Authoring rules

1. One concern per fragment. Family and model variants change wording for one concern; a new
   concern is a new fragment that a profile lists.
2. A tool is named only through `{{tool:<module>}}`, never as a literal, and only inside a
   `{{#tool:<module>}}…{{/tool:<module>}}` section or in a fragment whose profile entry
   `requires` that module. The coherence test renders every profile for every subset of its
   environment's tools.
3. Keep the heading literals the tests pin: `# Workers (optional)` in the delegation
   fragments, `# Finishing` in `finish`, and the sentence beginning `Every task ends with a`
   verbatim.
4. Dense prose in the Anthropic register: one rule per bullet, concrete nouns, no filler;
   placeholders `{{workspace}}`, `{{date}}`, `{{os}}`, `{{scratch}}`, `{{tool_names}}`,
   `{{model}}`, `{{family}}`.
5. Project policy (how work moves in a given repository) belongs in that repository's
   `AGENTS.md`, which the host appends after the prompt; the prompt carries behaviour that
   holds in every workspace.
6. A change to a fragment is a change to every profile that lists it: render each affected
   profile and read the result before committing (`p1 env show <environment>` once the
   assembler lands; before that, any script applying the rules in the specification).

## Provenance

The V1 text descends from the October 2026 Amp prompts (the GPT-6 and Opus 5.5 coding
prompts for `main`, the Fable prompt for `fable`, Puck for `manager`) and from the
previous p1 prompts (`environments/<name>/prompt.md`: tool mechanics, the finish contract,
workers and workflows). The lineage analysis and the fragment library live in the owner's
local prompt-lab, outside this repository.
