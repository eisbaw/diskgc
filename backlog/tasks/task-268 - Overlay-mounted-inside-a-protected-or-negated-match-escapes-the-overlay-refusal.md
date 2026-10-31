---
id: TASK-268
title: >-
  Overlay mounted inside a protected or negated match escapes the overlay
  refusal
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
Full review 2026-10-31 on master 6f14dd7 (tmp/reviews/codex-full-6f14dd7.md; mped full review in the TASK-266-window notes); static reading, not reproduced. Codex P1, static config, 0 events: protect /proj/target; an overlay at /proj/target/debug/deps/view with upperdir /cache/upper; select /cache/upper/cruft. Cargo discovery does not descend into deps/view, both compared paths are ext4, refuse_overlay passes, places differ by device; deleting the upper changes the protected subtree. executor/policy.rs refuse_overlay / overlaps_in_filesystem. Fix: include mounts nested beneath protected/negated matches (also through aliases) in the overlay uncertainty check; refuse unless backing-directory overlap is ruled out (parse upperdir/lowerdir or refuse). Model test red first.
<!-- SECTION:DESCRIPTION:END -->
