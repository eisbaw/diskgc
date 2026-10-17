---
id: TASK-39
title: 'Library: /tmp and Nix leftovers profiles, verified'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-17 11:12'
labels:
  - wave-4
  - config
  - library
dependencies:
  - TASK-38
  - TASK-37
  - TASK-17
references:
  - docs/reference.md#built-in-profile-library
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: these are the profiles the motivating survey found. Ship pytest-tmp, nix-shell-rc, nix-shell-tmpdir, tmp-entry, mktemp-dir, claude-code-tmp, chromium-tmp, playwright-tmp and node-compile-cache with examples, golden sets, verified_with, and real dormant nix-shell / nix develop session tests.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each profile has examples, a golden set and verified_with from a real system
- [ ] #2 Dormant nix-shell and nix develop sessions keep their TMPDIR dirs (container test)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Dogfooding run 2026-10-17 (read-only probe of LinuxHost plus ncdu of $HOME, /tmp, /var/tmp; full report in the session scratchpad dogfood/). Proposed profiles: containers-image-tmp (/var/tmp/buildah*, /var/tmp/container_images_storage*: 68.5G here, single-file blobs left by buildah/podman; only the generic tmp-entry catches them today) and nix-user-cache (~/.cache/nix: tarball-cache, gitv3, eval-cache-v*; 4.1G, refetched on demand). /tmp had 147,557 top-level entries (101k nix-shell rc dirs, 38k nix-shell.*); a full walk took 265 s at idle IO priority, which is the PRD's 100k-entry case. 'pytest-tmp' matches were all nested inside nix-shell temp dirs.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
