---
id: TASK-284
title: >-
  Retro: TASK-44..TASK-272 batch (fourth phase3.1 window: deployment, per-user
  lock, consolidation, release identity, protect holes, watch contract)
status: Done
assignee: []
created_date: '2026-11-01T05:26:00Z'
labels:
  - retro
  - process
dependencies: []
---

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Query: tools/retro.py parsing over master verification notes, merges after 2026-10-31T03:24Z (a6646d0 .. 48c906e). Run 2026-11-01.

| metric | value |
|---|---|
| merges | 8 shas, 13 tasks (44, 277, 270, 60, 45, 280, 268+269, 272+257+279+258) |
| cycle time | median 1.9 h; TASK-270 18.2 h and TASK-45 17.8 h span a session end (agents stopped, resumed from WIP commits; no work lost) |
| review rounds | 1 on every deletion/contract merge (mped + Codex in parallel); 0 on TASK-60 (harness) |
| blockers found in review rounds | TASK-44: 2; 277: 1 contract + 3 Codex; 270: 1; 45: 2; 268/269: 4 (Codex); 272 batch: 2; 280: 0 |
| post-merge findings in deletion-path code | 2 static-config holes (TASK-267 in TASK-242 code, TASK-268 in TASK-260 code), found by the 10-task Codex full review, although both merges had mped + Codex rounds |
| found_by | review 3, user 1 (TASK-277: non-root reclaim needed root provisioning, found by Mark dogfooding 0.1.0), walk 0, async-qa 0 |
| container run per irreversible merge | 8 of 8 (e2e-one, fast, or full e2e) |
| full e2e | TASK-45, TASK-280 (incl. VMs); master QA a0a970e fast 238/238 |

Experiments from Retro TASK-266:
- mped + Codex in parallel before deletion-path merges: KEPT (rigour-adding), but the target (0 post-merge findings) was missed: 2 static-config holes escaped both reviewers and were found by the periodic Codex full review. The periodic whole-codebase review is what caught them; keep it.
- Disk budget + container run per merge + warm worktree reuse: KEPT (8/8 irreversible merges had a container run; QA no longer starved; tmp/ cleaned by dogfooding diskgc).
- Extend the property/model generator before fixing: EXTEND (TASK-268 extended the model with overlays first and reproduced on a real kernel; blockers per round unchanged at 1-4).

Decisions (applied in the skill calibration):
1. Walk environments include an UNPROVISIONED non-root start: the test images provision /run/diskgc/lock, so no walk ever exercised a fresh user's first real run; Mark found it on the host (TASK-277). Every user-facing release walk runs the real (non-dry) start path once as a non-root user with nothing provisioned. Judged by: found_by user at the next retro (target 0).
2. Review briefs for protect/mount/storage changes ask explicitly for 'other mount types and aliases' (overlay, FUSE, ecryptfs/aufs/shiftfs, loop, bind, private propagation, other namespaces): both escaped holes were alias/mount-type classes. Judged by: post-merge findings in that class (target 0).
3. Session ends: agents commit WIP at every stop (already a rule) AND the orchestrator records the agent's worktree path + branch in the task notes before a long wait, so a resumed session can re-create the worktree and resume the agent (done ad hoc this window for t270/t45). Judged by: tasks restarted from scratch after a session end (target 0).
<!-- SECTION:FINAL_SUMMARY:END -->
