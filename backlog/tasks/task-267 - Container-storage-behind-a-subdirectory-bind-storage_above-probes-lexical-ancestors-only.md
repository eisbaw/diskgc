---
id: TASK-267
title: >-
  Container storage behind a subdirectory bind: storage_above probes lexical
  ancestors only
status: To Do
assignee: []
created_date: '2026-10-31T03:39:00Z'
updated_date: '2026-11-01T11:10:00Z'
labels:
  - safety
  - irreversible
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Full review 2026-10-31 on master 6f14dd7 (tmp/reviews/codex-full-6f14dd7.md; mped full review in the TASK-266-window notes); static reading, not reproduced. Codex P1, static config, 0 events: bind a rootless Podman store's overlay/ to /mnt/layers; a user-owned cache inside /mnt/layers/<id>/diff/ is admitted by discovery, re-match and the final checks, because the graphroot markers (storage.lock, overlay-layers) are outside the lexical path. engine/container_storage.rs storage_above. Fix: translate roots through mountinfo (as TASK-260's places) and probe backing ancestors through an accessible view of the same filesystem; fail closed when storage ancestry behind a subdirectory bind cannot be verified. Model test red first (bind_root exists since TASK-260). Applies to the TASK-242 backport question too.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Periodic full review #2 on master 794e0e1 (2026-11-01; Codex tmp/reviews/codex-full2-794e0e1.md + mped), static reading, not reproduced. mped BLOCKER, same class: a ROOT that is a bind of a directory inside container storage (e.g. /mnt/l = bind of ~/.local/share/containers/storage/overlay/<id>/diff) escapes storage_above, which walks the root's lexical ancestors and asks only the mount point's directory; translate the root through the mount's root field (TASK-260 place logic) and check reachable ancestors on that device; document if uncheckable (not refusing every btrfs subvolume root).

2026-11-01: in progress in tmp/worktrees/t267 (branch t267-bind-places).

2026-11-01: implemented in tmp/worktrees/t267 (t267-bind-places, e3bd4bb, not merged): storage_above also asks every directory above the root's place that the mount table shows (places::places_above). Red-first vectors walk/container-storage-{behind-a-subdirectory-bind,cargo-root-behind-a-subdirectory-bind}; real-kernel container test a_root_bound_from_inside_container_storage_is_refused (fails with the guard disabled). Limit documented: places no mount shows (btrfs top level, host dirs above a container volume) are not asked.
<!-- SECTION:NOTES:END -->
