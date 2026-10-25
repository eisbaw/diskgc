---
id: TASK-147
title: >-
  Human ensure output: raw epoch seconds, each skip printed twice, no sizes or
  outcome in the dry run
status: Done
assignee: []
created_date: '2026-10-24 21:10'
updated_date: '2026-10-25 01:02'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-143 walk (tmp/walks/2026-10-24/11-ci-dryrun.txt, tmp/walks/2026-10-24/12-ci-exhausted.txt, tmp/walks/2026-10-24/23-laptop-dryrun.txt).
- Skip lines: 'skip not_idle: .../target: last build at 1792875820 s, 56 s before now; idle_for 86400 s required'. A user wants 'last build 56s ago; needs idle 1d'.
- In a terminal (stderr and stdout together) the dry run prints every skip twice: the stderr selection log and the stdout plan table.
- The dry-run summary says '11 candidate(s), in the order ensure would take them' but not what a reclaim would free or whether the goal is reachable; skipped rows have no size, so a laptop owner cannot see what idle-7d would gain vs idle-1d.
- Exhausted summary 'kept /home/tester/src/important-lib/target: kept' repeats the reason as the text.
Repro: just walk-up; diskgc ensure --dry-run --min-free 2G --select 'profile:cargo-target && filter:idle-1d' --roots /home/tester/src /home/tester/src
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Also (tmp/walks/2026-10-24/24-laptop-tmp.txt): the dry run's 'held by deleted files: 1 process(es), 0 B; top: pid 4274 (python3.14, 0 B)' reports a holder of zero bytes (a live pytest's deleted temp file): noise; omit holders of 0 B from the human summary.

Inconsistent: the 'checking' progress line says 'last build 1 h ago' (human) while skip lines say 'last build at 1792875820 s, 56 s before now' (tmp/walks/2026-10-24/15-ci-hook-reclaim.txt vs 11-ci-dryrun.txt).

Implemented on branch t155 (04b7d8e..a372a43, not merged): report/outcome.rs folds actions/skipped (live) and plan (dry run) into one verdict per candidate; report/human.rs renders it. Walked 2026-10-25 in walk-diskgc-t155 (release-01-candidate of t155, seed 1): tmp/walks/2026-10-25-t155/ (11, 12, 15, 23, 24, 25, 26; *-before = 0.1 binary on the same data where the step is read-only). Follow-ups: TASK-160 (sizes for refused dry-run rows), TASK-161 (pid holding .cargo-build-lock).

Merged to master as 1ee71fa on 2026-10-25T01:02Z (rebased twice; just check exit 0, 1061 passed).
<!-- SECTION:NOTES:END -->
