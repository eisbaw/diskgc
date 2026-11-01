---
id: TASK-263
title: >-
  Reclaiming modes under hidepid or a non-initial PID namespace: refuse or give
  in_use_unknown instead of a silently weakened in-use scan
status: To Do
assignee: []
created_date: '2026-10-31T02:34:00Z'
updated_date: '2026-11-01T11:07:00Z'
labels:
  - safety
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the TASK-119 design review (mped, 2026-10-31): ProtectProc=invisible and PrivatePIDs=yes are what systemd-analyze security recommends; an operator adding them to the system template (CI exposure) or running diskgc in such an environment makes the in-use scan miss processes (hidepid hides newgrp/sg processes; a private PID namespace shows only the unit's own). The code already records both facts at startup (PidNamespace { initial, hidepid }) but uses them only for liveness contracts. Decide: under hidepid on the held /proc, reclaiming modes refuse at start (exit 2 naming the cause) or every candidate is in_use_unknown; for a non-initial PID namespace, a rule that keeps the rootless container test harness working (the e2e containers run in their own PID namespace by design). PRD 'Limits' already names hidepid; this makes the weakening loud instead of silent. Applies to 0.1 too (check the released behaviour first).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01: in progress in tmp/worktrees/t263 (branch t263-inuse-visibility).

2026-11-01 implemented on t263-inuse-visibility (f270c4e + follow-up), unmerged, awaiting the orchestrator's review round. Rule (implementer's choice, for Mark to confirm; docs/safety.md In use, 'Visibility: a process list that cannot be complete'): ensure (dry run too) and a reclaiming watch refuse at start, exit 2 cause safety, under hidepid on the held /proc or outside the initial PID namespace, root or not; unreadable facts exit 3; watch --tally-only, check, survey, profiles unaffected. Every scan re-reads the facts before its first listing and after its last read: failed/changed/failing -> every candidate in_use_unknown. Test-only exception: a development build inside a marked test container (gate's cached decision) takes its own PID namespace as complete (never hidepid, never Owner PID); compiled out of release-candidate. Evidence: model inuse_visibility + watch + vectors inuse/{hidepid,pid-namespace-*}; real kernel host dry runs (dev build, outside containers): unshare --user --pid --mount-proc -> exit 2 naming the namespace; hidepid=invisible procfs in a userns -> exit 2 naming hidepid; plain host dry run exit 0. Containers (one at a time, all PASS): ensure_is_refused_under_hidepid, release_candidate_refuses_in_its_own_pid_namespace, fs_reclaim_never_deletes_a_socket_or_fifo, owner_pid_candidates_are_kept_when_diskgc_has_its_own_pid_namespace, release_candidate_cargo_busy_then_reclaims_real_target; release runner suite 10/10 (every container now --pid=host, serialised); bite: without that runner rule the release cargo dry-run test FAILs with the namespace refusal. Behaviour change vs 0.1.0: CI hooks that ran diskgc inside a job container now get exit 2.
<!-- SECTION:NOTES:END -->
