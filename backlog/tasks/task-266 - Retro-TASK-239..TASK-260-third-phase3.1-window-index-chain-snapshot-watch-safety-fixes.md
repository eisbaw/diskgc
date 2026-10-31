---
id: TASK-266
title: >-
  Retro: TASK-239..TASK-260 (third phase3.1 window: index chain, snapshot,
  watch, safety fixes)
status: Done
assignee: []
created_date: '2026-10-31T03:31:00Z'
labels:
  - retro
  - process
dependencies: []
---

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Query: tools/retro.py parsing over master verification notes, merges after 2026-10-30T22:00Z (ab891e2 .. 3f8a1e7). Run 2026-10-31 by the orchestrator.

| metric | value |
|---|---|
| merges | 9 shas, 10 tasks (239 AC1, 31, 30, 242, 231+236+32, 252, 119, 260) |
| cycle time | median 1.3 h (excl. TASK-31's 120.7 h, a start date carried from a multi-day session gap); slowest real: TASK-242 2.1 h (review blocker + rebase over TASK-31), TASK-32 1.8 h (9 must-fix items), TASK-252 1.6 h |
| review rounds | 1 on every deletion-path merge; every round found blockers: TASK-30 5, TASK-242 1 (+2 high), watch 9 must-fix, TASK-252 1, TASK-260 2, TASK-119 1 |
| post-merge findings | 3 single-event holes in TASK-31 found by a Codex review after an mped-only round -> TASK-252 (a refix of 82cb9af) and TASK-30 |
| found_by | review 4 (TASK-30 Codex 3, 252, 260, 263 filed), walk 2 (TASK-242 host tally; watch UX kinks TASK-265), async-qa 0, user 0 |
| walked | 5 of 9 shas; watch walked only after disk recovered (container tests + walk on 12387c1) |
| QA lines | ab891e2 full e2e-all pass; 12387c1 partial (fast 232/232 + 6 targeted container tests); 6 irreversible shas between them have no own qa line (cumulatively covered at 12387c1 by the fast suite only) |
| binding constraint | disk: 34 -> 7.1 GiB free; container tests, QA and the watch walk were blocked for ~5 h; recovered to 17 GiB by Nix min-free GC, not by us |

Experiments from Retro TASK-250:
- Implementers commit WIP when they stop: KEPT (0 dirty worktrees without a live agent at this retro; every brief carried it).
- Standing master QA after irreversible merges (target >= 80% of irreversible shas): NOT MET (0 of 6 own lines; disk) -> EXTEND with the disk rule below.
- Live-host tests assert per branch: KEPT (0 fast-check failures not reproducible solo this window).

Process changes (applied now, in the skill calibration):
1. Deletion-path review round = mped + Codex in parallel on a frozen snapshot, before merge (was a gotcha after TASK-31; now the rule). Judged by: blockers found after merge in deletion-path code (target 0; this window 3, all from the one mped-only round).
2. Disk budget: reuse warm worktrees by switching branches (saved a fresh ~5-10 GB target per task this window); never start a container/VM job or a new worktree below 15 GiB; when a merge lands and disk >= 15 GiB, run the fast suite + that merge's container tests immediately, before starting the next implementer. Judged by: irreversible shas without a container run at the next retro (target 0).
3. Index/deletion-path tasks extend the index_authority (or relevant) property generator with the new mechanism BEFORE fixing: TASK-252's extended generator found a third bug the review missed. Judged by: review blockers per deletion-path round (this window median 1-2).
<!-- SECTION:FINAL_SUMMARY:END -->
