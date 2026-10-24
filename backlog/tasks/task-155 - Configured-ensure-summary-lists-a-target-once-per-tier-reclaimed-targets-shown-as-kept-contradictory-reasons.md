---
id: TASK-155
title: >-
  Configured ensure summary lists a target once per tier: reclaimed targets
  shown as kept, contradictory reasons
status: To Do
assignee: []
created_date: '2026-10-24 22:12'
labels:
  - shipping
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-143 walk, laptop owner journey (config with tiers stale.cargo-stale idle-7d and pressure.cargo-recent idle-1h).
- tmp/walks/2026-10-24/25-laptop-reclaim.txt: cargo-recent reclaimed fw-nrf/target and four agent worktree targets, yet the met summary says '7 kept: not_idle; examples: /home/tester/src/fw-nrf/target, .../agent-3224c438/target, ...' (cargo-stale's skips of those same targets) and '2 kept: protected; examples: /home/tester/src/important-lib/target (1 more)' (one target, counted per tier). A user reads that fw-nrf was kept.
- tmp/walks/2026-10-24/26-laptop-in-use-kept.txt: exhausted summary lists 'kept webapp/target: in_use' and 'kept webapp/target: not_idle', 'kept agent-slow/target: locked' and '... not_idle': one target, two contradictory reasons; '6 kept' for 3 targets.
JSON skipped[] has one row per (tier, candidate), which is defensible for machines (the contract says rows per tier), but the human summary and its kept count must be per candidate: omit candidates a later tier reclaimed, and give each remaining one the reason from the last tier that judged it (or list the reasons per tier explicitly). Also: a locked target gives process_blockers null; naming the Cargo pid holding .cargo-build-lock would tell the user which build to wait for.
Repro: just walk-up; wait an hour after walk-reset (idle-1h); write the two-tier config (tmp/walks/2026-10-24/laptop-from-reference.yaml minus the non-0.1 parts) and run diskgc ensure --min-free 2300M /home/tester.
<!-- SECTION:DESCRIPTION:END -->
