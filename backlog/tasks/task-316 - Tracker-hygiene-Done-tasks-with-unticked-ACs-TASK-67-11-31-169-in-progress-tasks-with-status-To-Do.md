---
id: TASK-316
title: >-
  Tracker hygiene: Done tasks with unticked ACs (TASK-67, 11, 31, 169);
  in-progress tasks with status To Do
status: Done
assignee: []
created_date: '2026-11-02T11:33:00Z'
updated_date: '2026-11-02T14:48:00Z'
labels:
  - process
  - backlog
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-02: 'ensure we tackle these findings ... we need more real-world VM tests, so we need to bake into the e2e tests that we do clean up afterwards.' From COMPASS consult #3 (master c286b3b). COMPASS: Done tasks with unticked acceptance criteria undermine release audits: TASK-67 (0.1 release, 0/7), TASK-11 (0/5), TASK-31 (0/6), TASK-169 (0/2). For each: tick the ACs that the verification notes prove (cite the note), or reopen / record why not. Also set status In Progress on tasks being worked (TASK-295, 310 say in progress in notes but are To Do).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-02: in progress in tmp/worktrees/t315 (branch t315-release-criteria).

2026-11-02 result (branch t315-release-criteria). Each tick cites its proof in that task's notes.
- TASK-11: AC1, 2, 3, 5 ticked (verification note 75c89c1; docs/testing.md Cargo race tests; the race 5/6 tests' --pause-at/--kill-at). AC4 not: the shared target-dir variant runs races 1, 2, 4, 8 only and race 6 there is unrun; split build-dir tests are vacuous on 1.90.0 (TASK-13 H15, H16).
- TASK-31: all six ticked (notes 82cb9af, c0885a2; model tests in diskgc-model/tests/index_reclaim.rs; AC4's 100k run predates the review fix and no full tier has re-run it).
- TASK-67: AC1-5 and 7 ticked (v0.1.0 tag message; RC notes a11e77b, af3d503; release suite tests at v0.1.0). AC6 not: no note shows the unproved safety-critical contracts got evidence before the tag; Mark's tag is the decision, his to confirm.
- TASK-169: neither ticked; items 1 (io_uring, overlay upper/work), 2, 3, 6 neither reproduced nor argued; carried by TASK-303; the pending loop policy is TASK-296, alias limits TASK-305.
- DoD items were not assessed (out of this task's scope).
- In-progress tasks with status To Do: TASK-295 and TASK-310 were set In Progress in f37938a and are now Done (c0885a2, 96b4875); no other To Do task's notes say in progress except TASK-305, which is In Progress.
- Other Done tasks with unticked ACs (backlog task list --plain -s Done: 12 of the Done tasks with ACs, these four included). The other eight, not yet reconciled: TASK-28 (0/3), TASK-29 (0/4), TASK-30 (2/3), TASK-44 (4/5), TASK-117 (0/3), TASK-257 (0/2), TASK-39 (0/2), TASK-71 (0/2). Release-relevant for 0.2 (index, snapshot, watch --tally-only, units): 28, 29, 30, 44, 117, 257. Unverified leads: TASK-30 AC1 has the e2e test watch_tally_only_never_deletes_and_a_second_watch_exits_2; TASK-29 AC2 names the report's index block, which is still null (TASK-244), so it may be unmet; TASK-71 AC1 may be met by the PRD's Key decisions (checkout guard on by default).

2026-11-02: merged (ff), docs and backlog only; just check exit 0 (run 1 hit the TASK-317 flake).
<!-- SECTION:NOTES:END -->
