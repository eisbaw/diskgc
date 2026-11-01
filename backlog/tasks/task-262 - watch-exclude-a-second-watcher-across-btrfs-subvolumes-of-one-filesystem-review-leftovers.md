---
id: TASK-262
title: >-
  watch: exclude a second watcher across btrfs subvolumes of one filesystem;
  review leftovers
status: Done
assignee: []
created_date: '2026-10-31T01:54:00Z'
updated_date: '2026-11-01T15:39:00Z'
labels:
  - watch
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the TASK-32 review (Codex 10, mped N1): (1) watcher exclusion and the snapshot key use st_dev, so two watchers on different btrfs subvolumes of one filesystem both start (host lock still serialises deletions; double tally). Key on the mountinfo superblock device (TASK-30's space guard already does); the model needs subvolume support (a file st_dev differing from its mount) to test it. (2) A device-mismatch cycle record has no available-space numbers although the sample succeeded. (3) No test that a slow index wait eats into the cycle deadline (the inline model driver never waits).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Periodic full review #2 on master 794e0e1 (2026-11-01; Codex tmp/reviews/codex-full2-794e0e1.md + mped), static reading, not reproduced. superseded by the one-filesystem-identity task (btrfs subvolumes, fs_key).

2026-11-01: merged with TASK-286 after one review round (verification note on the merge).
<!-- SECTION:NOTES:END -->
