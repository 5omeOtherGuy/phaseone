---
adr: 102
title: Credential index rebuilds keep earlier identities and expose a test clock
status: proposed
date: 2026-09-29
deciders: lead
supersedes: []
superseded_by: []
sources: ["https://github.com/5omeOtherGuy/phaseone/issues/481", "https://github.com/5omeOtherGuy/phaseone/pull/483"]
---
# ADR-0102: Credential index rebuilds keep earlier identities and expose a test clock

## Context

Issue #481: on a coarse filesystem clock (Depot CI runners) a file written in the same tick as the credential index read leaves the directory stamp unchanged, so a cached `ProtectedIndex` was reused and missed a new credential. Refusing reuse of such racy stamps makes rebuilds more frequent, and Codex (PR #483) showed that every rebuild dropped the identities of the replaced index: a credential linked onto an ordinary path whose protected name was then removed became readable and mutable through the alias.

## Decision

`p1-workspace`'s `ProtectedIndex` gains three public methods: `build_with_clock` (the read clock that decides the racy-stamp margin, injectable so tests need no sleeps), `stamps_unchanged` (stamp equality only, for a walk detecting a change during one request) and `retain_identities_of` (a rebuild of the same policy keeps refusing the earlier index's credential identities). `still_current` additionally refuses stamps within `DIRECTORY_STAMP_SAFETY_MARGIN` (2 s) of the read.

## Consequences

Every rebuild site that replaces an index (the module-runtime search cache, workspace mutation refresh) retains earlier identities, which also closes the same alias gap on fine-grained clocks. A cached index may refuse an inode that was a credential earlier in the process's life; that over-refusal is intended. Directories changed in the last two seconds are re-walked on each check, a bounded cost. `build_with_clock` is a test seam in the public interface because the module-runtime tests need it across crates.

## Alternatives considered

A racy check only in the module-runtime cache (rejected: the workspace mutation path reuses stamps the same way). Refusing every file while a stamp is racy (rejected: ordinary reads right after a key change would fail). A cargo feature for the test clock (rejected: more build configuration for one seam).

## Evidence

`file_services::tests::directory_index_cache_reuses_and_rebuilds_on_change` failed on every Depot run of #480 and passed on Depot with this fix (throwaway branch run, build job, 2026-09-29). Unit tests `a_directory_changed_inside_the_build_margin_is_never_reused` and `a_rebuild_keeps_refusing_a_credential_whose_protected_name_was_removed` in `crates/p1-workspace/src/policy.rs`.
