---
adr: 27
title: Workers are dispatched directly, with a machine-wide bounded pool
status: accepted
date: 2026-09-20
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [D14, D15, scripts/fanout.py]
---
# ADR-0027: Workers are dispatched directly, with a machine-wide bounded pool

## Context

D14 (owner+lead): the owner rejected a relay model. The ledger quotes the owner: "I don't
like haiku relays … use our workers directly." The Workflow tool was rejected because its
scripts can only spawn Claude agents, so a non-Claude worker would need a Claude relay. D15
(lead) then bounded the pool from measurements.

## Decision

`scripts/fanout.py` runs a job list as direct `pi-worker` processes and prints one JSON
summary; its exit is the completion signal for the batch. No relay or supervisor model. The
pool accepts any queue length but runs at most 6 live workers, and starts no new worker while
MemAvailable is below 1500 MB; both bounds count every `pi-worker` on the machine, not only
this batch's.

## Consequences

The lead is woken by the batch's exit instead of polling, and a long queue still runs in
a bounded pool. Workers cost about 115-190 MB resident each, so the bounds protect 7 GB of
RAM; compiles are serialised separately by ADR-0014. The bound must be revisited with
measurements, not by raising the number.

## Alternatives considered

A Claude relay via the Workflow tool (rejected by the owner); 20 live workers (rejected
by D15's measurement: about 3-3.5 GB against roughly 2.3 GB available with 4.4 GB already
swapped).

## Evidence

D14 records the verified run `20260920-000458-1394036` (one deepseek job, $0.0006,
completion notification received). D15 records the 115-190 MB measurement. The rationale and
bounds are in `../../scripts/fanout.py`'s docstring; commit 103127c added the script and
26728ef widened the bound to the whole machine.
