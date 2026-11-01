---
id: TASK-288
title: >-
  Per-user lock: NFS home needs a writable fd for flock; an NSS lookup error
  silently falls back to $HOME
status: To Do
assignee: []
created_date: '2026-11-01T05:37:00Z'
updated_date: '2026-11-01T19:50:00Z'
labels:
  - safety
  - lock
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #2 on master 794e0e1 (2026-11-01; Codex tmp/reviews/codex-full2-794e0e1.md + mped), static reading, not reproduced. Codex 3: with no host lock and no runtime dir, the fallback lock under an NFS home is opened read-only, but exclusive NFS flock needs write access, so reclaims fail before deletion every time. Fix: reopen the checked per-user lock writable without truncation, keeping identity and permission checks. mped SHOULD 4: LinuxHost::lock_home does account_home(uid).ok().flatten(), so an NSS error falls back to $HOME silently and is cached; two runs with different $HOME take different locks; safety.md says $HOME only when the lookup finds no entry. Fix: fall back only on Ok(None); on Err log or fail.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01: in progress in tmp/worktrees/t287 (branch t287-small-fixes).
<!-- SECTION:NOTES:END -->
