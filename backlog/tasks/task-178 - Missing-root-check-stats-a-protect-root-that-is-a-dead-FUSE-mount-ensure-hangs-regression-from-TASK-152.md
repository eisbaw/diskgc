---
id: TASK-178
title: >-
  Missing-root check stats a protect root that is a dead FUSE mount: ensure
  hangs (regression from TASK-152)
status: To Do
assignee: []
created_date: '2026-10-25 03:49'
labels:
  - release-blocker
  - regression
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by async QA on 47d9bd4 (2026-10-25): fs_protected_dead_fuse_root_is_refused_without_opening_it hangs deterministically (gate TIMEOUT; e2e-one hung 900 s). Hung diskgc in D state at fuse_get_req in statx(fd -> /tmp/fuse-dead, AT_EMPTY_PATH|AT_NO_AUTOMOUNT|AT_SYMLINK_NOFOLLOW). Probable cause (not bisected): 3e23b0f (TASK-152) added refuse_missing_roots in crates/diskgc/src/main.rs, which under --select still checks protect roots via resolve_in (crates/diskgc-core/src/resolve.rs), opening and statx-ing the mount point before protect discovery refuses it from mountinfo. Fix: the missing-root check must judge a root that is (or lies under) a mount point from mountinfo without touching it, consistent with docs/safety.md Mounts first. Evidence: tmp/e2e-logs-batch/47d9bd4/, e2e-one-47d9bd4-dead-fuse.log, hung-diskgc-proc.txt. Also check west_generated_output_uses_surviving_source_anchor duration growth (62 s fe0ab5f, 106 s ce6a0cd, 201 s 47d9bd4).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 fs_protected_dead_fuse_root_is_refused_without_opening_it passes again (e2e-one)
- [ ] #2 A model vector or unit test proves the missing-root check never stats a root that mountinfo shows as a mount point it must not open
- [ ] #3 Root cause confirmed (bisect or reasoning recorded); west test duration growth explained or filed
<!-- AC:END -->
