---
id: TASK-267
title: >-
  Container storage behind a subdirectory bind: storage_above probes lexical
  ancestors only
status: To Do
assignee: []
created_date: '2026-10-31T03:39:00Z'
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
