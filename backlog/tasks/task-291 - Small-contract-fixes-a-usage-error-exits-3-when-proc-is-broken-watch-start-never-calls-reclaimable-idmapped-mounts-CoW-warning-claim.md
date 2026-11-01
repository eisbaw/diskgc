---
id: TASK-291
title: >-
  Small contract fixes: a usage error exits 3 when /proc is broken; watch::start
  never calls reclaimable; idmapped mounts; CoW warning claim
status: To Do
assignee: []
created_date: '2026-11-01T05:37:00Z'
updated_date: '2026-11-01T19:50:00Z'
labels:
  - contract
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #2 on master 794e0e1 (2026-11-01; Codex tmp/reviews/codex-full2-794e0e1.md + mped), static reading, not reproduced. Codex 8 + mped SHOULD 3: main.rs Invocation::UsageError arm calls startup() for every command but check/profiles, so a mistyped flag with a broken /proc exits 3 host (keep only the kernel-version check before reporting the parse error). mped SHOULD 8: watch::start never calls entry.rs reclaimable (latent until TASK-36). SHOULD 10: idmapped mounts not handled (ownership walk sees the mapped uid; owner's processes via the plain path invisible): parse the idmapped flag and refuse, or document. SHOULD 11: PRD says copy-on-write filesystems are 'warned about' but no warning exists (btrfs/zfs/bcachefs, overlay whiteouts): a one-line note by fs_type, or drop the claim.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01: in progress in tmp/worktrees/t267 (branch t267-bind-places).

2026-11-01 correction: the 'in progress in tmp/worktrees/t267' note above was put here by mistake (the orchestrator's brief named TASK-291 but meant TASK-290, contains_mount). This task's own items are untouched.

Periodic full review #3 on master fc33d98 (2026-11-01; Codex tmp/reviews/codex-full3.md + mped), static reading. mped SHOULD 6 again: main.rs UsageError arm runs startup() for every first word except check/profiles; a typo can come back as exit 3. Report usage errors before startup().

2026-11-01: in progress in tmp/worktrees/t287 (branch t287-small-fixes).
<!-- SECTION:NOTES:END -->
