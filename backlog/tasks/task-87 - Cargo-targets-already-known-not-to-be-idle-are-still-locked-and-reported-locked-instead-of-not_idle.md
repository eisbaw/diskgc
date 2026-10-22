---
id: TASK-87
title: >-
  Cargo targets already known not to be idle are still locked, and reported
  locked instead of not_idle
status: Done
assignee: []
created_date: '2026-10-19 02:52'
updated_date: '2026-10-22 22:17'
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
Found by the CI operator journey (task-12, round 2: the hook's default selection, `profile:cargo-target && filter:idle-1d`, over targets seconds old, with a goal nothing could meet). The tally measures each target's last build before locking ("reclaiming /tmp/builds/idle-1/target (est. 4.5 MiB, 31 entries, last build 1792378297)"), but the idle filter is applied only in the Reclaim protocol's step 2, under the target's locks.

When it matters: targets are taken oldest first and the reclaim stops at the goal, so diskgc reaches (and locks) a fresh target only when the run is below --min-free and the older targets cannot bring it to --target-free. With the hook's defaults (--target-free = --min-free) that is only runs that end exhausted (exit 1). In those runs every target the idle filter will refuse is locked anyway, and a target whose build is running is reported `locked` instead of `not_idle`, which tells the operator less (the journey's default case pins `locked` today).

The locks are only tried, never waited for, so this costs no deadlock; but a Cargo of that target starting while diskgc holds them waits for the whole step-2 walk (measured 0.49 s for 100,026 entries untraced, safety.md), for a target diskgc was never going to delete. Refusing a target the tally already measured as not idle is the safe direction (the step-2 recheck under the locks stays for the ones that pass); reference.md (`--tally-budget`) lists what the tally refuses and could add not_idle. The output also prints "reclaiming X" for such targets (task-86).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A target whose measured last build fails the tier's idle filter is skipped not_idle without taking any of its locks (container test: /proc/locks or a held lock shows no attempt)
- [x] #2 reference.md (--tally-budget) and safety.md (Reclaim protocol, step 2) say so; the step-2 recheck under the locks is unchanged
- [x] #3 The CI operator journey's default-selection case expects not_idle (not locked) for the paused build's target
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-23 implementation in isolated repo-local worktree tmp/worktrees/task87-idle-precheck: prechecks tallied Cargo build age before target locks and preserves locked recheck for accepted/untallied targets. Focused nix develop cargo test -p diskgc-linux --lib --locked passed 141/141 (one helper ignored); focused ModelHost no-lock test passed 1/1; disposable Podman journey journey_ci_operator_frees_space_with_ensure@1.99.1 passed 1/1, with paused Cargo holding all target locks before not_idle and later lock-time recheck still producing locked. Parallel QA and MPED reviews GO for the diff. Exact nix develop -c just e2e is still running; no commit and no host deletion.

Final gate 2026-10-23: exact nix develop -c just e2e exited 0 after code was stable: format, Python, Clippy, workspace/doctests, Nix fast 219/219 in 73.7s, Docker fast 7/7, and five VM checks. QA and MPED parallel reviews GO; git diff --check clean. AC1 proven by ModelHost zero target-flock trace plus paused-build /proc/locks container journey; AC2 docs updated; AC3 journey now expects not_idle and retains later locked recheck. No host deletion.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
