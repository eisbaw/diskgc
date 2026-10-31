---
id: TASK-242
title: >-
  cargo-target discovery descends into container storage
  (~/.local/share/containers/storage overlay layers)
status: Done
assignee: []
created_date: 2026-10-26 07:06
updated_date: '2026-10-31T00:32:00Z'
labels:
  - safety
  - ux
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Host dry-run tally 2026-10-26 (master 5daf3ea, release build, ensure --dry-run --select profile:cargo-target --roots /home/mpedersen /): 8 candidates under ~/.local/share/containers/storage/overlay/<layer> skipped as unreadable (subuid-owned layer dirs). Only the permission bits stop it: a readable image layer holding a Cargo target (CACHEDIR.TAG + lock) would be planned for deletion, corrupting a container image. Same exposure for any home-root fs profile and for Docker's /var/lib/docker, containerd, Nix store paths (read-only), VM images. Decide: a built-in discovery exclusion for container/VM storage roots (and their reporting: one skip per root, not one unreadable per layer), or a mount/ownership rule. Evidence: tmp/host-tally/cargo-target.json.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-30: merged (ff) at 9937817 after one review round (mped + Codex). Container e2e not re-run after the review fixes (disk); TASK-253 (Docker 29 marker measurement) and TASK-254 (unrecognised layouts) open. 0.1.x backport is Mark's call.
<!-- SECTION:NOTES:END -->
