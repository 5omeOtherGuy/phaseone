## Intent

<!-- What the change is for, and why. `Fixes #n` for the issue it resolves. -->

## Changed behavior

<!-- What behaves differently after this pull request, for users and for code. "None" for a pure refactor or docs fix. -->

## Constraints touched

<!-- The repository constraints this change touches (AGENTS.md: architecture, frozen tests, credentials, the gate) and how it keeps them. A changed public interface or contract, a new dependency or a reversed decision links its accepted ADR; otherwise write "none". -->

## Evidence

<!-- The commands you ran and their results, e.g. the focused tests and `scripts/pre-push.sh`. Say what you did not run. -->

## Sensitive paths

<!-- Does this change a sensitive path named in the AGENTS.md profile, a public interface, security-sensitive code, or a document's rules or executable instructions? If yes, an independent review (another agent or the owner) reads the diff against AGENTS.md's constraints and is recorded on this pull request with the head commit it reviewed, before auto-merge is armed; a later push needs a new review. -->

- [ ] No sensitive path changed
- [ ] A sensitive path changed, and the independent review is recorded on this pull request against the reviewed head

## Checklist

- [ ] Up to date with `main`
- [ ] `scripts/pre-push.sh` passes, or the reason it does not is above
- [ ] User-visible change: entry under `## [Unreleased]` in `CHANGELOG.md`, and docs updated
- [ ] No credentials, tokens, private prompts or personal email addresses in the diff
