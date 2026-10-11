# Tests and CI audit

The full pull-request gate passed in **9m 25s**, versus successful baselines of
24m 29s and 28m 09s, with all 360 default executables and every gate obligation retained.
The target is a green required check within 600 seconds of workflow creation, including
setup and aggregation. Documentation and clean-merge reuse paths are measured separately.
The baseline bottleneck was serial execution of workspace test binaries, followed by
restoring and rebuilding stale caches. Cold-cache timing remains unknown.

## Measured baseline

Two successful full runs establish the baseline. Times include setup and cache restore;
workflow elapsed time includes classification, scheduling and the final aggregate job.

| Run | Workflow elapsed | Checks | Module tests A | Module tests B | Workspace tests |
| --- | ---: | ---: | ---: | ---: | ---: |
| [38146489661](https://github.com/5omeOtherGuy/phaseone/actions/runs/38146489661) | 24m 29s | 5m 42s | 4m 19s | 5m 28s | 24m 12s |
| [38146774375](https://github.com/5omeOtherGuy/phaseone/actions/runs/38146774375) | 28m 09s | 6m 38s | 4m 25s | 3m 45s | 27m 54s |

The second run's workspace step spent 101.4 seconds compiling and 1362.95 seconds executing
its default test binaries and doctests. Its feature-off acceptance spent another 3.64 seconds.
The cache restore took 75 seconds, module builds 81 seconds and sandbox provisioning 21 seconds.

| Slow binary | Execution seconds | Finding |
| --- | ---: | --- |
| p1-assembly lead_prompt_coherence | 364.83 | Exhaustive subset rendering in an unoptimized harness |
| p1-host route_files | 314.78 | Shared conformance suite across six composed routes |
| p1-host anthropic_route | 125.97 | Composed provider conformance, including every fixture byte split |
| p1-host openai_route | 79.83 | Composed provider conformance, including every fixture byte split |
| p1-host cli | 40.97 | CLI assembly and component loading |

The conformance suites already use paused Tokio time. Their byte-split sweeps exercise every
boundary of nine fixtures; removing or sampling those cases would reduce acceptance coverage.
Loader compilation is already shared by verified digest within each process. Keeping the
libtest binary as the scheduling unit preserves that reuse and process-global test setup.
[Cargo documents serial target execution and each test's package working directory](https://doc.rust-lang.org/cargo/commands/cargo-test.html).

## Test inventory

Cargo metadata at baseline d3ddd4a1 reports 59 native workspace packages and 365 targets:
59 libraries, 298 integration targets, four binaries, three examples and one build script.
The guest workspace contains 45 packages, including 39 component packages. Guest components
are checked and built for the pinned wasm target and exercised through native host suites.
All native packages receive default unit, integration and documentation tests. Examples are
compiled by default; an example with test=true also runs. Clippy checks all native targets.

The following inventory counts targets, not individual assertions. Zero integration targets
can still mean extensive inline unit tests, particularly in p1-module-runtime and guest logic.

| Package | Unit libraries | Integration targets | Unit binaries | Compiled examples |
| --- | ---: | ---: | ---: | ---: |
| `p1-acp` | 1 | 14 | 0 | 0 |
| `p1-assembly` | 1 | 15 | 0 | 0 |
| `p1-attachment-local` | 1 | 1 | 0 | 0 |
| `p1-auth` | 1 | 21 | 0 | 0 |
| `p1-context` | 1 | 6 | 0 | 0 |
| `p1-contracts` | 1 | 1 | 0 | 0 |
| `p1-core` | 1 | 13 | 0 | 0 |
| `p1-finish-guest` | 1 | 0 | 0 | 0 |
| `p1-github-guest` | 1 | 0 | 0 | 0 |
| `p1-hook-shadow` | 1 | 1 | 0 | 0 |
| `p1-host` | 1 | 75 | 1 | 0 |
| `p1-journal` | 1 | 8 | 0 | 0 |
| `p1-json-order` | 1 | 0 | 0 | 0 |
| `p1-live` | 1 | 1 | 0 | 0 |
| `p1-mcp-client` | 1 | 3 | 0 | 0 |
| `p1-model-profile` | 1 | 0 | 0 | 0 |
| `p1-module-protocol` | 1 | 3 | 0 | 0 |
| `p1-module-runtime` | 1 | 0 | 0 | 0 |
| `p1-module-tests` | 1 | 42 | 2 | 0 |
| `p1-provider-anthropic` | 1 | 11 | 0 | 0 |
| `p1-provider-conformance` | 1 | 1 | 0 | 0 |
| `p1-provider-http` | 1 | 1 | 0 | 0 |
| `p1-provider-openai` | 1 | 7 | 0 | 0 |
| `p1-provider-openai-chat` | 1 | 6 | 0 | 0 |
| `p1-question-guest` | 1 | 0 | 0 | 0 |
| `p1-read-guest` | 1 | 0 | 0 | 0 |
| `p1-read-output-guest` | 1 | 0 | 0 | 0 |
| `p1-redact` | 1 | 0 | 0 | 0 |
| `p1-shell-guest` | 1 | 0 | 0 | 0 |
| `p1-shell-job-guest` | 1 | 0 | 0 | 0 |
| `p1-skill-fs` | 1 | 1 | 0 | 0 |
| `p1-testkit` | 1 | 0 | 0 | 0 |
| `p1-todo-guest` | 1 | 0 | 0 | 0 |
| `p1-todo-session` | 1 | 0 | 0 | 0 |
| `p1-tool-delegate` | 1 | 2 | 0 | 0 |
| `p1-tool-edit` | 1 | 1 | 0 | 0 |
| `p1-tool-edit-logic` | 1 | 0 | 0 | 0 |
| `p1-tool-finish` | 1 | 4 | 0 | 0 |
| `p1-tool-ls` | 1 | 0 | 0 | 0 |
| `p1-tool-patch` | 1 | 1 | 0 | 0 |
| `p1-tool-patch-logic` | 1 | 0 | 0 | 0 |
| `p1-tool-question` | 1 | 1 | 0 | 0 |
| `p1-tool-read` | 1 | 3 | 0 | 0 |
| `p1-tool-read-output` | 1 | 1 | 0 | 0 |
| `p1-tool-search` | 1 | 3 | 0 | 0 |
| `p1-tool-search-logic` | 1 | 0 | 0 | 0 |
| `p1-tool-shell` | 1 | 9 | 0 | 0 |
| `p1-tool-shell-job` | 1 | 0 | 0 | 0 |
| `p1-tool-skill` | 1 | 0 | 0 | 0 |
| `p1-tool-tests` | 1 | 3 | 0 | 0 |
| `p1-tool-todo` | 1 | 0 | 0 | 0 |
| `p1-tool-workflow` | 1 | 2 | 0 | 0 |
| `p1-tool-write` | 1 | 0 | 0 | 0 |
| `p1-tool-write-logic` | 1 | 0 | 0 | 0 |
| `p1-tui` | 1 | 23 | 0 | 3 |
| `p1-usage` | 1 | 0 | 0 | 0 |
| `p1-workers` | 1 | 0 | 0 | 0 |
| `p1-workflow` | 1 | 12 | 0 | 0 |
| `p1-workspace` | 1 | 2 | 0 | 0 |

The baseline has 24 Python suites. The gate's manual list omitted test_journal_timing.py and
test_gate_reuse.py; pre-push already discovered both. The gate now discovers every test_*.py,
including the scheduler's new coverage and failure controls.

Thirteen ignore annotations are intentional existing policy: nine module benchmarks, a
process-contention child helper, two workflow CPU/RSS measurements and one runtime output
measurement. The helper is invoked explicitly by its parent test. No ignore annotation,
frozen acceptance assertion, fixture or conformance input changes in this work.
The p1-live suite returns before provider access unless P1_LIVE=1; CI does not enable it.
Unit and conformance transports remain scripted. Tests with real sleeps deserve later cleanup,
using explicit synchronization or fake time; this change does not reinterpret pinned behavior.

## Gate and workflow coverage

| Obligation | Where it runs after simplification |
| --- | --- |
| Native and guest formatting | checks phase |
| Native all-target clippy with denied warnings | checks phase |
| Toolchain pins and wasm guest clippy | checks phase |
| Fresh component build, validation, digests, worlds and allocated imports | checks phase; fresh builds also precede both test partitions |
| Usable bubblewrap | shared setup for every full-gate job, plus gate probe |
| Every native default test target and doctest | workspace and modules partitions, discovered by Cargo |
| Host execution without delegation | workspace partition |
| Core isolation and serde_json ordering | checks phase |
| Frozen module/unsafe boundaries and zero native fallbacks | checks phase |
| Secret scan and ADR/index validation | checks phase; docs path also runs these |
| Every Python regression suite | checks phase, automatic discovery |
| Candidate for exact HEAD and installed file:// smoke with isolated HOME/config | checks phase |
| Required success, rejecting skipped/failed/cancelled partitions | aggregate gate job |

The checks phase prints CHECKS PASSED, never GREEN. Default gate.sh retains all checks and
its final GREEN marker. The PATH cargo wrapper that silently returned success for workspace
tests is removed; CI uses an explicit --checks invocation and requires the test matrix.
The matrix has fail-fast disabled so one failure does not cancel another partition's findings.
Each binary keeps its complete libtest harness, package working directory and package metadata
environment. Doctests stay with Cargo/rustdoc, and example compilation stays in Cargo's default
no-run build. Missing expected artifacts fail closed. Binaries have a bounded timeout; any
nonzero exit, timeout or sandbox/runtime-directory skip makes the partition red.

## Simplification and cache changes

Setup appears once in a composite action: pinned Rust, pinned wasm-tools, cache restore and
sandbox provisioning. Six full-gate runners share it: checks, one module-only partition,
and four balanced workspace shards. This increases runner count to shorten the critical
path; it does not claim a measured reduction in total runner cost. Cargo verifies the full
target inventory on every shard before execution. Committed measured costs assign every
workspace target exactly once; new targets get a default weight. Cached timings affect
execution order only, never membership, because jobs could restore different cache versions.
Workspace shards run at most two binaries concurrently; the module partition runs four.
Cargo/rustdoc runs every partition's doctests once, in shard 1, which also owns the
workspace's feature-off acceptance. All shards are required; fail-fast is disabled.

The sum of executed job durations was 39m 48s and 42m 48s in the two successful
baselines, versus 41m 47s in the third changed run. These are measured job runtimes,
not billed charges. The shorter critical path has not established lower total resource
use. Monetary cost remains unknown; measure it from billing rather than multiplying
elapsed workflow time by an assumed price.
The successful fourth run used 46m 28s of summed job time. In particular, each workspace
shard currently compiles the full default target set to preserve Cargo's feature and
compilation behavior before selecting its executable subset. Reducing duplicated
compilation without changing feature unification or example compilation is a follow-up
opportunity; it is not used to claim the current target is met.

The assembly package's development profile uses optimization level 3 for prompt processing
and its exhaustive acceptance harness. The component runtime uses level 3 and Wasmtime
level 1 for repeated host-side instantiation. Within the exhaustive chunking check, one
paused executor per fixture reuses its blocking pool across byte splits; every split still
builds a fresh provider, transport, request and cancellation token. Timing-specific checks
retain isolated runtimes. No new Rust crate or extra toolchain is introduced. Local pre-push requests a build slot only
when selected package tests or optional clippy will actually compile; formatting and script
tests no longer wait behind unrelated builds.

The guest builder validates all selected package metadata, invokes Cargo once for the whole
selected graph, then publishes and validates each component with the existing atomic output
rules. Cargo can compile independent packages concurrently, rather than paying 39 serial
Cargo invocations. Partial builds retain their existing whole-set attestation refusal.

Old cache keys contained lockfiles only. Once a key existed, successful main jobs never saved
again until a lockfile changed. The audited workspace cache was 2,596,831,058 bytes; the four
gate entries totaled 5,008,874,967 bytes. Restores also fell back across unrelated partitions.
New keys include partition, lockfiles and commit, with same-partition restore prefixes. Main
saves current successful artifacts; PRs only restore. Workspace shard 1 saves the common
complete compilation cache; other workspace shards do not race to save the same key.
Incremental compilation is disabled in
CI and restored incremental/candidate scratch is excluded when saving. Legacy same-partition
caches can bootstrap the transition. [GitHub documents immutable keys and restore-prefix behavior](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching).

Caching every main commit may still cause storage eviction. Exact cache size and restore
cost must be measured from subsequent runs; a lockfile-only cache cannot be refreshed in place.
Cold-cache elapsed time remains unknown until a cold GitHub run is measured. A shorter timeout
alone does not satisfy the target: success and complete coverage must finish within 600 seconds.

## Other CI and test findings

Only gate and release workflows exist. Main requires gate, including administrators, with
strict/up-to-date checking disabled. Docs and clean-merge reuse retain ADR-0192 behavior;
they are not evidence of faster full test execution. Main pushes still receive the appropriate
classification and releases only follow a successful main push gate.

The release workflow separately builds a release-profile binary, builds components, stages
four assets and verifies existing assets before publication/repair. Its cache also uses a
lockfile-only immutable key and does not include modules/target. Release performance is a
separate optimization; its duration does not contribute to the required gate and is unknown
for this audit's changed implementation. Moving gate obligations to release would weaken the
merge contract and is not used to meet the target.

Known flakes are tracked in [#796](https://github.com/5omeOtherGuy/phaseone/issues/796)
(compaction workload on two-core runners) and [#837](https://github.com/5omeOtherGuy/phaseone/issues/837)
(shared workspace refusal). The shared-workspace harness now explicitly waits for both
queued writers before the parent's mutation. A single poll dispatched a blocking task but
did not establish that it had read the original file; the parent's write could land first,
correctly producing the newer diff-bearing refusal. Waiting on the existing gate's waiter
count preserves every assertion and pins the intended stale queued commit. No sleep or
production behavior change is introduced. Concurrency can expose shared-resource assumptions; such failures
must be repaired through synchronization rather than retries, ignored tests or reduced coverage.
The process adoption acceptance also keeps its 300ms deadline and every assertion, but uses
paused time: a blocking guard prevents automatic clock advancement while the real shell
starts, and the test advances the clock after observing `ready`. It still checks timeout,
live adoption, preserved output and exactly one successful completion notification.
Test bodies contain both sleep-based timing and explicit synchronization. A source scan cannot
establish that every such wait is unnecessary; behavior changes to frozen cases need owner approval.

## Validation and continuing measurement

The first changed full run, [38151411772](https://github.com/5omeOtherGuy/phaseone/actions/runs/38151411772),
finished red in 18m 16s. Checks passed in 3m 59s and module tests in 4m 09s (45
executables, 383 passed, nine ignored). Workspace compilation took 136 seconds;
315 executable attempts took 953.53 seconds including compilation and doctests.
The optimized prompt sweep passed in 80.69 seconds. Concurrent provider binaries
regressed: Messages 562.47 seconds, Responses 324.26 seconds, and six chat routes
hit the 600-second binary timeout. The shared-workspace failure reproduced #837.
This run does not meet the objective. Host-side component instantiation is now
optimized in development/test builds as well as the existing compiler crates;
its measured effect is below.

The second run, [38153174607](https://github.com/5omeOtherGuy/phaseone/actions/runs/38153174607),
finished red in 17m 06s. Checks passed in 5m 02s and modules in 4m 04s; module
build/tests/doctests took 130.77 seconds. Workspace compilation took 179 seconds
and build/tests/doctests 890.49 seconds. Messages improved to 492.59 seconds and
Responses to 273.38 seconds; chat still timed out at 600.05 seconds. Shared workspace
reproduced #837 again. A separate runtime unit failure expected the shell's `ready`
output before its 300ms adoption deadline; it was not present under this run's load.
No assertion was changed or retried. This second run also misses the objective;
the final layout shards the workspace and reuses the chunking executor's blocking pool.

The third run, [38154585971](https://github.com/5omeOtherGuy/phaseone/actions/runs/38154585971),
finished in **9m 21s**, but remained red. All 360 executables ran: 4327 workspace
cases passed, one failed, four were ignored; modules had 383 passed and nine ignored.
Workspace shards covered 1 + 104 + 105 + 105 executables and took 7m 42s, 8m 59s,
7m 47s and 9m 04s including setup. Modules passed in 3m 58s. Chat conformance
passed in 189.05 seconds; Messages in 242.80 seconds; Responses in 210.27 seconds.
The shared-workspace synchronization passed. The runtime's shell-readiness assertion
failed again; it now uses explicitly advanced fake time as described above. Checks
completed every obligation and printed CHECKS PASSED, then the workflow's strict skip
guard rejected a sandbox-skip marker intentionally printed by a new negative-control
test. That control's output is now captured; its rejection assertion stays in place.
This is evidence that the sharded layout fits 600 seconds, not a successful gate yet.

The fourth run, [38155607631](https://github.com/5omeOtherGuy/phaseone/actions/runs/38155607631),
passed the full gate in **565 seconds (9m 25s)**: workflow creation 16:28:30 UTC,
final required gate completion 16:37:55 UTC on 2026-10-11. Every required phase was
green; all 315 workspace and 45 module executables ran. Binary results were 4711
passed, zero failed and 13 existing ignored cases. Every partition's doctests and
the host's feature-off acceptance also passed. Checks included all 25 Python suites,
both formatting checks, clippy, component validation and the exact-HEAD installation
smoke. Chat conformance passed in 241.80 seconds; Messages in 243.32 seconds;
Responses in 214.34 seconds. Runtime readiness and shared-workspace coordination
passed with every original assertion intact. This used restored legacy caches;
it is a complete successful full gate, not a documentation/reuse fast path.

Local formatting and focused script controls remain useful checks. The local pre-push
waiter was stopped during this optimization phase after the owner's question about its
necessity; the authoritative complete validation and timing run on GitHub.

The scheduler's regression controls cover complete disjoint target discovery, new packages,
missing artifacts, concurrency, package execution context, failed binaries, sandbox skips,
compile failure, hung binaries and doctest failure. Disjoint shard assignment also verifies full coverage and stable membership regardless of
input order; secondary shards verify compilation coverage without repeating doctests.
Successful sandbox skips are rejected in doctest output as well as native binary output.
Gate harness tests still exercise every required step's failure and add the partial-phase
refusal to claim GREEN. Component publication,
sandbox, release and all other existing script suites remain required.

The same local exhaustive prompt suite passed in 99.14 seconds with optimization versus
741.75 seconds for the baseline binary. Both ran with other workstation sessions active,
so GitHub full-gate timings remain the deciding measurement.

The successful full PR run confirms the 600-second objective for that measured run.
Continuing measurements, including the merged main run and its cache saves, are recorded
on [PR #857](https://github.com/5omeOtherGuy/phaseone/pull/857). Record workflow creation
to final gate completion, each job's setup/build/run/save duration, all discovered target
counts, and the scheduler's slow-target table. Retain queue and cache costs instead of
reporting just test execution; a cold-cache run and future queue delay remain unknown
until measured, and this audit does not guarantee an upper bound on GitHub scheduling.
