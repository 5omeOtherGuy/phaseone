---
adr: 184
title: Client MCP values mask only their own server, and every masked value is counted, shown and recorded
status: accepted
date: 2026-10-11
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [799, 695, 142, 766]
---
# ADR-0184: Client MCP values mask only their own server, and every masked value is counted, shown and recorded

## Context

ADR-0182 registered every value an ACP client sends in `mcpServers` (each stdio `env` value, each HTTP header value, and the last word of each) in the session's one `SecretSet`, the set every assembled tool masks. A harmless client value such as `production` or a folder path was then replaced by `<redacted:secret:N chars>` in every file read, search and shell output of the session, and the model edited from text it could not see. The replaced-value count reached the user only as a total per turn, without the tool, and no record kept it. The owner ordered the fix on 2026-10-11 ("Implement the fix", #799, from the review of #695's ASSUMPTION 1): a client value masks only its own server's tools, and masking is never silent.

## Decision

1. **Scoped client values.** A value the client sends for server S goes only into S's child process or S's requests, never into p1's own environment, so p1's shell and file tools cannot see it; it can come back only through S's replies. It is registered in a set scoped to S (`SecretSet::scoped`: S's values plus the session's set, read live), never in the session's set. S's tools are wrapped with that set (`RedactingTool::with_secrets`), their declarations are checked against it (`check_declaration`), and S's long results are masked with it before the output store keeps them. Values p1 passes from its own environment by name (`env_pass`, `bearer_token_env`) stay in the session's set and are masked in every tool, as p1's shell can see them. A session start failure is masked with every value the client sent for any server, as a server's error may echo one.
2. **Counted per tool and per call.** `MaskCounter` counts every replacement under the tool's model-facing name (`add`, `take_by_tool`), and a result's replacements also under its call id (`add_call`, `take_call`). This covers every masking a tool wrapper does: credential shapes, registered credentials and client values, in results, descriptions and declarations.
3. **Shown once per turn.** The host's existing display-only notice (`MaskNoticeSink`, an `AgentEvent::ProviderNotice` after the turn ends) names the total and each tool, `masked N possible credential value(s) in tool output: <tool> <n>, …`, never a value. The line front end prints it on stderr (`· …`); the ACP adapter already maps `ProviderNotice` to the session process's operator log (`p1 acp: …`, ADR-0048).
4. **Recorded with the result.** The host's journal sink (`AssemblyLines::masked_sink`) reads a `ToolFinished` record's count by its call id and writes it on the record's version-3 line as `"masked": N` beside `at_ms` (`JsonlJournal::commit_masked`); `p1_journal::load` reports it in `Loaded::masked`, parallel to the records. The field is absent when nothing was masked, so every other line is byte-identical, and a version-1 or version-2 file keeps its format. `JournalRecord` and `RecordBody` are unchanged.

## Consequences

A client's non-secret values no longer hide text from the model in p1's own tools, and a client value in another server's declaration no longer fails the start. If p1's own tools ever read text that S produced (a file S wrote, a log), S's values in it are not masked there; that is the price of the rule, and the client's values never reach p1's environment. The model's own text is no longer masked against client values: it can hold one only if the user typed it. Every masked value shows up in a per-turn notice and in the journal, so a reader can tell where masked text came from. An ACP client sees the notice only in the session process's log: no ACP update type carries an operator notice without making it assistant content. A long MCP result's masking, done by the output store before the cut, is counted against its call too.

## Alternatives considered

A new `RecordBody` variant for the count: the core owns record sequencing and its projection, and the frozen suites would need more than a match arm. A field on `ToolFinished` or `ToolResultItem`: every record literal, including the frozen acceptance suites, would change. Counting `<redacted:…>` markers in the journal afterwards: a literal marker a tool printed would count, and display-only masking leaves no marker in a record. Mapping the notice to an ACP `agent_message_chunk`: it would become assistant content, which ADR-0048 rules out. Keeping client values session-wide and only reporting the count: the model would still edit from text it cannot see.

## Evidence

`cargo test -p p1-redact` (`a_scoped_set_masks_its_own_values_and_the_outer_sets_but_not_the_reverse`, `the_decorator_counts_per_tool_and_per_call_and_masks_the_set_it_was_given`); `cargo test -p p1-host --lib mcp::` (`a_client_value_masks_only_its_own_servers_tools`, the long-result store masks and counts per server); `cargo test -p p1-host --test acp_mcp_servers` (`a_client_value_masks_its_own_servers_output_and_not_another_tools`: the same string is masked in `mcp__sh__hello` and shown by `read`, the records count 1 and 0; `a_start_failure_echoing_a_client_value_reaches_the_client_masked`; a negative control that makes client values session-wide again fails both new tests); `cargo test -p p1-host --test mcp_servers` (an `env_pass` credential is masked in `read` too, the notice names `mcp__sh__hello 1, read 1` and no value, both records count 1); `cargo test -p p1-host --test redaction`; `cargo test -p p1-journal --test journal_version` (`a_masked_count_rides_on_a_version_3_line_only_when_not_zero`).
