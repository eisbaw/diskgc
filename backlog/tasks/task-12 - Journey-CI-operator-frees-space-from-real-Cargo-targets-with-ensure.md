---
id: TASK-12
title: 'Journey: CI operator frees space from real Cargo targets with ensure'
status: Done
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-19 03:19'
labels:
  - wave-1
  - journey
dependencies:
  - TASK-11
references:
  - PRD.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: exercise the product as a user end to end before building more. In a container with several real Cargo projects (some idle, one building, one with a running test binary), run diskgc ensure from a pre-job-style script as a CI operator would; read the human output and the JSON.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The busy and running targets survive, idle ones are deleted, the deletion oracle agrees
- [x] #2 Findings (confusing output, missing information, surprises) are filed as tasks, not fixed silently
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
AC #3 (the same journey on a Debian image with the static binary) moved to task-63, the Debian / static-binary e2e variant, which depends on this task (backlog restructuring for the 0.1 ensure MVP, 2026-10-18).

From task-10 (for the CI-operator journey): the runnable command is diskgc ensure --min-free 50G [--target-free 80G] --select 'profile:cargo-target && filter:idle-1d' --roots "$CI_BUILDS_DIR" "$CI_BUILDS_DIR" [--json]. Needs /run/diskgc/lock (exit 2 prints the two tmpfiles lines); PATH and --roots absolute; a config is optional with --select (version: 1 with profiles only; protect/owners refused, groups ignored). Human mode is silent when nothing was needed; --json always one object. Exit codes 0/1/2/3/75/124 implemented (130/143 not: task-54). Not yet: config tiers, unions, !filter/!profile, other kinds, --allow-root, index (no 'index' field), aggregated skips. A development build only reclaims inside the test containers.

Journey (2026-10-19): journey_ci_operator_frees_space_with_ensure (diskgc-e2e/src/journey_suite.rs), a Cargo test per toolchain in its tier, runs the example hook docs/ci-pre-job.sh as written (sh, traced, judged by the oracle) over five real projects in /tmp/builds: idle-1..3 (SMOKE, built a second apart; idle-1 holds a vendored .git), build-running (RACE, cargo build paused in build.rs, holding its locks by /proc/locks) and tests-running (RACE, cargo test whose test binary waits at its gate, no lock held), plus a deleted 4 MiB log a stale process holds. Cases: dry run (human); the hook's default selection (filter:idle-1d) with A's goal: exit 1, nothing deleted, idle and running targets not_idle, the paused build locked (task-87); hook A (min/target-free from statvfs + the harness's byte count: idle-1, idle-2 reclaimed, idle-3 untouched, exit 0 met); hook B (100%: idle-3 reclaimed, exit 1 exhausted); human-mode run; fresh runner without a builds dir (hook makes it, default mktemp report, exit 0); misconfigured hook (unwritable report path, relative or unset CI_BUILDS_DIR: exit 2 before diskgc runs); runner without the host lock (hook's own check, exit 2, both tmpfiles lines). Every exit 0/1 checked against the harness's statvfs and min-free before the exit code; the hook's summary line checked against the report. Busy targets unchanged object by object, lock inodes unchanged, both busy commands succeed afterwards, idle projects rebuild.
AC #1: met. AC #2: findings filed as task-82 (human summary), task-83 (empty targets re-reported as skipped), task-84 (missing host lock unnoticed while the goal is met; high, blocks task-67), task-85 (JSON for dashboards), task-86 (progress log wording), task-87 (targets known not idle are still locked), task-88 (stdout write error panics, exit 101; blocks task-67), task-89 (idle filter selecting old targets through the binary). task-26 depends on 83 and 85. Doc changes: docs/ci-pre-job.sh (new example), reference.md points at it, testing.md Journeys bullet.
Bites, round 1 (each alone on 1.99.1, restored by copy): (a) the exe-inside-the-target evidence rule removed: green; (b) step 2's in-use check under the locks removed: green, step 3's per-part Cargo heuristics stop it before the first unlink; (c) step 2 removed AND the per-part Cargo heuristics disabled (cargo_evidence ignored in delete_part): red, oracle: .fingerprint of the running test target deleted. QA confirmed (a)+(b) together stay green (3x on 1.99.1 and 1.90.0): rules 3-5 (cargo's cwd, LD_LIBRARY_PATH, CARGO_MANIFEST_DIR) in step 3 still keep the target; it goes red only when step 2 is removed and cargo_evidence returns nothing.
Bites, round 2: hook without its report-writability check: red (exit 1, the post-run check says "without writing its report"); default selection changed to profile:cargo-target: red (oracle); field() emptied: red (summary); empty count forced to 0: red at B; conclude() returning met while below: red at the statvfs check ("exit 0, but the harness sees ... below --min-free").
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
