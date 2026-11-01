---
id: TASK-299
title: >-
  Loop backing identity holes: idmapped bind over a root-owned image, deleted
  overlay name scoped to the wrong device, multi-device btrfs on loop missed by
  protect
status: To Do
assignee: []
created_date: '2026-11-01T16:39:00Z'
updated_date: '2026-11-01T16:40:00Z'
labels:
  - safety
  - irreversible
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #3 on master fc33d98 (2026-11-01; Codex tmp/reviews/codex-full3.md + mped), static reading. Codex P1 x3, static config, 0 events: (1) inuse/loops.rs by_name: a root-owned loop image inside a Cargo target exposed through an idmapped bind as UID 1000's; the owner-rule exemption dismisses it (original path UID 0) while the eligibility walk through the bind sees UID 1000 and deletes. Fix: exempt only after proving the file cannot appear owned by an allowed user through any candidate mount (idmapped mounts present -> keep in_use_unknown). (2) by_name deleted-name branch: loop configured through an overlay merged view, merged name removed, hard link survives on the underlying fs; unknown_on uses the overlay parent's device, the surviving link reports the underlying device, so its last name is deleted. Fix: keep ambiguity global for stacking filesystems unless aliases are proved; audit the ioctl branch for the same alias gap. (3) executor/policy.rs loop_images: every policy call passes None to on_loop, so only the mountinfo source device is checked; a second loop member of a multi-device btrfs whose image lies inside a protected Cargo target is missed. Fix: enumerate btrfs members via a held fd + FSID (/sys/fs/btrfs/<fsid>/devices) during overlap checks; incomplete membership fails closed. Model tests red first where expressible.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01: in progress in tmp/worktrees/t299 (branch t299-loop-identity).
<!-- SECTION:NOTES:END -->
