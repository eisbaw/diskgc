---
id: TASK-274
title: >-
  Refactor: split Reclaim per kind, central Skip-row builders, a Mounts value
  per candidate step, linux host.rs split
status: To Do
assignee: []
created_date: '2026-10-31T03:39:00Z'
updated_date: '2026-11-01T16:40:00Z'
labels:
  - refactor
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Full review 2026-10-31 on master 6f14dd7 (tmp/reviews/codex-full-6f14dd7.md; mped full review in the TASK-266-window notes); static reading, not reproduced. mped SHOULD 5-9 (top refactors 2+3): Skip rows hand-built in seven places (SkipEntry::candidate / tier_level); ~20 independent mountinfo reads in core, ~7 per candidate with protect (one Mounts value read once per step: mount_for, screen_chain, place_of, refuse_overlay); executor.rs imports union/negated/Docker helpers from the dry-run module plan.rs (move to executor/union.rs, docker_backend); executor.rs -> executor/reclaim/{mod,fs,cargo,docker,union}.rs; diskgc-linux host.rs (~6.6k lines): move command and Docker code out; Host trait (52 methods) into capability sub-traits; remove the unguarded public fs_kind::reclaim, reclaim_alternatives, cargo_target::reclaim (no-op policy) so a new kind cannot call them by mistake. Do after the one-entry-point refactor.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Periodic full review #2 on master 794e0e1 (2026-11-01; Codex tmp/reviews/codex-full2-794e0e1.md + mped), static reading, not reproduced. additions: move the mount-topology half of executor/policy.rs (overlaps_through_images, loop_images, refuse_overlay, stored_elsewhere, place_of, attachments) into a mounts module on the Mounts value (grep -rn '\.mountinfo()' crates/diskgc-core/src lists the reads; overlaps_through_images re-reads per loop depth); split diskgc-linux host.rs into procfs readers, command child runner (reusable by TASK-36 nix-store --gc), Docker query, config reader, state/per-user lock. See the one-filesystem-identity task.

2026-11-01 (TASK-292 review, mped): reading /proc/self/mountinfo once per check step is ~30% of the reclaim main thread after TASK-292. No safety decision needed for: collapsing to one read per documented step (safety.md names step 1 'read now', step 4 'read again', 'once more' before the delete) with a ModelHost counter test; and reusing the parsed table when the raw bytes are identical to the last read (profile kernel text production vs parsing first).

Periodic full review #3 on master fc33d98 (2026-11-01; Codex tmp/reviews/codex-full3.md + mped), static reading. mped: reorder and add ACs: (1) the Mounts value (places/ did the algebra; wrapper with by_id/by_dev indexes, generation, unchanged_for(older, paths); each documented step reads one Mounts at its top; model counter pins reads per step; TASK-294 then only changes where Mounts::read is called), (2) one policy decision (see the one-protect-decision task), (3) Reclaim split per kind folding indexed.rs's impl, (4) host.rs into capability sub-traits; move fs_uuid/btrfs_fsid/loop_devices out of state.rs into fsid.rs, (5) skip-row builders; move policy.rs's mount-topology half into a pure places::overlap. After the 0.2 RC.
<!-- SECTION:NOTES:END -->
