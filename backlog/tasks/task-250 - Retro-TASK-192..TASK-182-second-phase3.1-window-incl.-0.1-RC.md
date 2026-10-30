---
id: TASK-250
title: 'Retro: TASK-192..TASK-182 (second phase3.1 window, incl. 0.1 RC)'
status: Done
assignee: []
created_date: '2026-10-30T22:26:00Z'
updated_date: '2026-10-30T22:27:00Z'
labels:
  - retro
  - process
dependencies: []
---

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Correction (same day): the mount/FUSE experiment's stated metric was 'regressions found by async QA next window, target 0'. Async QA found 6 this window (TASK-167 Cargo lock names, TASK-190 test strings twice, release package version, TASK-197/199 probe, TASK-226), so the target was missed; none was a mount/FUSE/resolution regression, which is what the rule guards. Kept as a rigour-adding rule, judged next time on mount/FUSE/resolution regressions only.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Query: python3 tools/retro.py over master + release-0.1 verification notes, merges after 72b376c (TASK-178, end of Retro TASK-188) up to 41755fd (TASK-182). Run 2026-10-30 by the orchestrator.

| metric | value |
|---|---|
| merges | 26 (25 with started/merged) |
| cycle time | median 1.2 h; slowest TASK-182 7.3 h (overnight gap 23:20Z-06:37Z, not work), TASK-29+230 4.7 h (2 review rounds, deletion-adjacent tally engine), TASK-184 3.6 h (perf, laptop dry-run walks) |
| review rounds | 0: 11, 1: 12, 2: 2 (TASK-28, TASK-29) |
| refixes | 3: 636158b of ee643a6 (TASK-190 test strings), 14dc12b of 636158b (one more missed in_use string, hidepid), 29810c5 of 72fccd8 (release package version broke a VM check eval) |
| found_by | walk 8, review 5, async-qa 6, user 0 |
| walked | 15 yes, 10 no (test-only, design docs, internal engine) |
| async QA lines | 3 of 26 shas (b5bcb7a pass, 5878d68 pass, ee643a6 fail) + the RC QA on a11e77b/af3d503 |

Experiments from Retro TASK-188 / COMPASS 3:
- mount/FUSE container tests before merging resolution changes: KEPT (0 mount/FUSE regressions found by async QA; 182/196 mount-screen merges ran them).
- compare touched files before parallel implementers: EXTEND (no conflict data recorded; TASK-233 adds a conflicts key).
- perf measurements record load, <= 2 other implementers: EXTEND (load recorded on TASK-184/RC notes, band share not computable).
- COMPASS 3 (a) e2e-all stages recorded: KEPT (5878d68, RC). (c) every irreversible-surface sha gets async QA: NOT MET (3/26); the RC QA covered release-0.1 only, master after the RC (index chain, TASK-182/196) has no qa line. Counterweight check: late user-visible bugs found by async QA this window 6 vs 4 the window before, so no quality-reducing entry is kept on that basis; none of this window's entries reduced rigour.

Decisions (applied now, written to the skill's calibration):
1. Implementers commit WIP on their own branch whenever they stop (report, interruption, session end), never leave a fix round uncommitted: TASK-31's review-fix round sat uncommitted in tmp/worktrees/t31 for 4 days with its review findings only partly recorded. Judged by: worktrees with uncommitted changes and no live agent at the next retro (target 0).
2. Async QA on master resumes as a standing job: launch e2e-all on the master sha after every merge that touches an irreversible surface when none is running, record qa: on it. Judged by: qa-line coverage of irreversible shas (target >= 80%).
3. `just check` must not depend on host quietness: a live-host test whose assertions assume a settled /proc scan made the fast check red under load (TASK-239 AC1, found by the 10-27 review, not a gate). Live-host tests assert per branch of what the host can legitimately return. Judged by: fast-check failures not reproducible solo (target 0).
<!-- SECTION:FINAL_SUMMARY:END -->
