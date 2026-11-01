---
id: TASK-297
title: >-
  Retro: TASK-251..TASK-289 batch (fifth phase3.1 window: reclaim speed, host
  tests, bind places, in-use visibility, filesystem identity, release
  descriptor)
status: Done
assignee: []
created_date: '2026-11-01T16:30:00Z'
labels:
  - retro
  - process
dependencies: []
---

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Query: tools/retro.py parsing over master verification notes, merges after 2026-11-01T05:30Z (cf60cd6 .. 3264e8c). Run 2026-11-01.

| metric | value |
|---|---|
| merges | 7 shas, 11 tasks (251, 282, 292, 267+285+290, 263+169, 286 (+262), 289) |
| review rounds | 1 on every deletion/contract merge; 0 orchestrator rounds on TASK-282 (test tooling; implementer ran qa+mped) |
| blockers found in review rounds | 251: 3 contract; 292: 1 (claim wording); bind batch: 3 (incl. openSUSE refuses every root); in-use: 4 (incl. forgeable test exception, loop decoy); 286: 6 (incl. nested subvolume deletion, both reviewers); 289: 2 contract |
| found_by | review 4, walk 2 (host /tmp measurement -> 251, 292), implementer 1 |
| real-kernel container test per deletion merge | 5 of 5 (bind 3 tests, in-use refusals, lock-bind guard, TTFD, liveness) ; btrfs/bcachefs/loop model-only |
| async QA on master | fast suite after 4 merges (238-245 all pass) + release suite once; full just e2e once (TASK-289 branch) |
| interruptions | 2 agents stopped on an API network error; resumed from WIP commits at recorded worktree paths, no work lost |

Experiments from Retro TASK-284:
- Non-root unprovisioned real start in release walks: NOT EVALUATED (no release walk this window); carry.
- Review briefs ask about other mount types and aliases: KEPT; in-window catches in that class (openSUSE snapper EACCES, nested subvolumes, loop on btrfs/partitions, decoy overmount, bcachefs s_dev) all pre-merge; post-merge escapes unknown until the next periodic review.
- Record worktree path + branch before long waits: KEPT (both network-error stops resumed with no loss).

Orchestrator errors this window (honest record): briefed the bind batch with TASK-291 instead of TASK-290 and mis-numbered three tasks in a user summary (implementer caught it); the in-use loop design rested on a false premise (udisks chowns loop devices) that the implementer disproved from source; a fast-forward failed once because a backlog commit landed after the branch's rebase.

Decisions (applied in the skill calibration):
1. Briefs quote the task's title from 'backlog task view N --plain', not from memory. Judged by: wrong-task briefs (target 0).
2. Orchestrator-designed fixes list their premises as checks the implementer must verify and report (the udisks case worked only because the implementer checked). Judged by: merged designs resting on an unverified premise (target 0).
3. Merge with 'git -C <worktree> rebase master && git merge --ff-only' as one command after the last backlog commit, never a separate rebase earlier. Judged by: failed fast-forwards (target 0).
<!-- SECTION:FINAL_SUMMARY:END -->
