---
id: TASK-291
title: >-
  Small contract fixes: a usage error exits 3 when /proc is broken; watch::start
  never calls reclaimable; idmapped mounts; CoW warning claim
status: To Do
assignee: []
created_date: '2026-11-01T05:37:00Z'
labels:
  - contract
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #2 on master 794e0e1 (2026-11-01; Codex tmp/reviews/codex-full2-794e0e1.md + mped), static reading, not reproduced. Codex 8 + mped SHOULD 3: main.rs Invocation::UsageError arm calls startup() for every command but check/profiles, so a mistyped flag with a broken /proc exits 3 host (keep only the kernel-version check before reporting the parse error). mped SHOULD 8: watch::start never calls entry.rs reclaimable (latent until TASK-36). SHOULD 10: idmapped mounts not handled (ownership walk sees the mapped uid; owner's processes via the plain path invisible): parse the idmapped flag and refuse, or document. SHOULD 11: PRD says copy-on-write filesystems are 'warned about' but no warning exists (btrfs/zfs/bcachefs, overlay whiteouts): a one-line note by fs_type, or drop the claim.
<!-- SECTION:DESCRIPTION:END -->
