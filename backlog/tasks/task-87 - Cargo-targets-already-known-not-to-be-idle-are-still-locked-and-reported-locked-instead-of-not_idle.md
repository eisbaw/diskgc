---
id: TASK-87
title: >-
  Cargo targets already known not to be idle are still locked, and reported
  locked instead of not_idle
status: To Do
assignee: []
created_date: '2026-10-19 02:52'
updated_date: '2026-10-19 03:08'
labels:
  - cli
  - safety
  - wave-2
dependencies: []
references:
  - docs/safety.md
  - diskgc-e2e/src/journey_suite.rs
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the CI operator journey (task-12, round 2: the hook's default selection, `profile:cargo-target && filter:idle-1d`, over targets seconds old). The tally measures each target's last build before locking ("reclaiming /tmp/builds/idle-1/target (est. 4.5 MiB, 31 entries, last build 1792378297)"), but the idle filter is applied only in the Reclaim protocol's step 2, under the target's locks. So every run takes every Cargo lock of every target the filter will refuse anyway (on a CI runner with idle-1d: every target used today, on every job), and a target whose build is running is reported `locked` instead of `not_idle`, which tells the operator less.

The locks are only tried, never waited for, so this costs no deadlock; but a Cargo of that target starting while diskgc holds them waits for the whole step-2 walk (measured 0.49 s for 100,026 entries untraced, safety.md), for a target diskgc was never going to delete. Refusing a target the tally already measured as not idle is the safe direction (the step-2 recheck under the locks stays for the ones that pass); reference.md (`--tally-budget`) lists what the tally refuses and could add not_idle. The output also prints "reclaiming X" for such targets (task-86).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A target whose measured last build fails the tier's idle filter is skipped not_idle without taking any of its locks (container test: /proc/locks or a held lock shows no attempt)
- [ ] #2 reference.md (--tally-budget) and safety.md (Reclaim protocol, step 2) say so; the step-2 recheck under the locks is unchanged
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
