---
id: TASK-178
title: >-
  Missing-root check stats a protect root that is a dead FUSE mount: ensure
  hangs (regression from TASK-152)
status: Done
assignee: []
created_date: '2026-10-25 03:49'
updated_date: '2026-10-25 05:48'
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
- [x] #1 fs_protected_dead_fuse_root_is_refused_without_opening_it passes again (e2e-one)
- [x] #2 A model vector or unit test proves the missing-root check never stats a root that mountinfo shows as a mount point it must not open
- [x] #3 Root cause confirmed (bisect or reasoning recorded); west test duration growth explained or filed
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 implementer (branch t178, commit ca4dc9d, not merged). Root cause confirmed by model backtrace (not bisected; e2e-one not rerun at 3e23b0f to avoid a 900 s hang): refuse_missing_roots -> resolve_in -> openat2/statx on the dead mount. Fix: a root/PATH is missing only if screen_mount_chain accepts its chain and resolve_in says ENOENT/ENOTDIR. Same class fixed in check's warn_missing_roots (stat following symlinks -> mountinfo screen + openat2 O_PATH RESOLVE_NO_SYMLINKS). ModelHost gains mount dead: true (any lookup/statx/getdents/read/statfs/statvfs reaching it fails the test). Vector config/dead-fuse-protect-root-never-touched fails on master code, passes on branch; cli test check_warns_only_about_an_absent_supplied_root. e2e-one fs_protected_dead_fuse_root_is_refused_without_opening_it: PASS 5.63 s. west_generated_output_uses_surviving_source_anchor alone on branch: PASS 107.6 s at load avg ~25; its diskgc runs take 17-89 ms (47d9bd4: 31/89 ms); the time is the fixture's two real west/CMake builds, so the growth is host load and suite concurrency (47d9bd4 suite overran its 300 s budget), not a product slowdown. Gate: just check exit 0 (1121 tests), e2e-clippy and e2e-tests exit 0. mped-architect: ship, no blockers.

Merged to master as 72b376c on 2026-10-25T05:48Z.
<!-- SECTION:NOTES:END -->
