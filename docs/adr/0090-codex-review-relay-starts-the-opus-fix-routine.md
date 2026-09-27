---
adr: 90
title: Codex review relay starts the Opus fix routine
status: proposed
date: 2026-09-27
deciders: owner+lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0090: Codex review relay starts the Opus fix routine

## Context

OpenAI Codex reviews every pull request and submits a PR review only when it has findings; a clean result is an issue comment. The owner ordered that Codex keeps reviewing and a cloud Opus routine fixes its findings on the PR branch ("Keep codex reviewing the PRs and Opus fixing them", 2026-09-27). Claude routine GitHub triggers accept only pull-request and release events, so a `pull_request_review` subscription never starts the routine; until now it ran hourly or by hand.

## Decision

`.github/workflows/codex-review-relay.yml` runs on `pull_request_review` (submitted) when the reviewer is exactly the bot `chatgpt-codex-connector[bot]` (type `Bot`; any account can review a public PR, so a login substring would let others start the routine), the PR is not a draft and its head branch is in this repository (a fork PR's token is read-only and cannot label; its findings wait for the hourly or manual run); it removes and re-adds the label `codex-reviewed` with the job's `GITHUB_TOKEN`. The routine subscribes to `pull_request.labeled`, works only on a PR that carries that label, and removes only that label when done.

## Consequences

A Codex review with findings starts a fix run within seconds instead of at the next hourly run. Each fix push can draw a new Codex review and so a new run; the loop ends when Codex finds nothing. The job needs `issues: write` and `pull-requests: write` and holds no secret. Removing the workflow or the label returns the repository to hourly or manual fixing.

## Alternatives considered

The routine API trigger (`/fire` with a bearer token) needs a token minted in the claude.ai web interface and stored as a repository secret. A local poller was ruled out by the owner's order to stop all loops that wake an agent.

## Evidence

A human-added `codex-reviewed` label on merged PR #385 started the routine within two seconds (2026-09-27 10:36 UTC); the run named PR #385 from its trigger context and stopped because the PR was merged. Codex review and comment bodies on #382, #385, #386 and #388 show a review only for findings.
