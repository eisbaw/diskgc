---
id: TASK-65
title: /tmp built-in profiles for 0.1 (verified)
status: To Do
assignee: []
created_date: '2026-10-17 23:55'
updated_date: '2026-10-18 12:42'
labels:
  - wave-2
  - profiles
dependencies:
  - TASK-17
references:
  - docs/reference.md#built-in-profile-library
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: 0.1 ships "the verified /tmp and Cargo built-ins" (PRD Scope, Releases), for the /tmp laptop owner (task-22). Pulled out of task-39: the verified /tmp subset only, all with the plain delete action.

Profiles (names from docs/reference.md, Built-in profile library):
- pytest-tmp: pytest-of-* in /tmp, /var/tmp, and one level down (nix-shell / mktemp dirs)
- nix-shell-rc: /tmp/nix-shell-*-*, /tmp/nix-develop-*-* (rc dirs left by every shell start)
- nix-shell-tmpdir: /tmp/nix-shell.* (a shell's TMPDIR)
- mktemp-dir: /tmp/tmp.*
- tmp-entry: any top-level entry of /tmp, /var/tmp
- containers-image-tmp (from the task-39 dogfooding note, 2026-10-17): /var/tmp/buildah*, /var/tmp/container_images_storage* (68.5G on the dogfood host, blobs left by buildah/podman). It is not yet in the reference.md table; adding the row is part of this task, and it ships only if verified like the others.

Dogfood findings to verify against (task-39 note): /tmp had 147,557 top-level entries (101k nix-shell rc dirs, 38k nix-shell.*), a full walk took 265 s at idle IO priority (the PRD's 100k-entry case); all pytest-tmp matches were nested inside nix-shell temp dirs.

Stays in task-39: claude-code-tmp, chromium-tmp, playwright-tmp, node-compile-cache and nix-user-cache (~/.cache/nix, not /tmp scope).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each profile ships in builtin.yaml with examples and golden match sets (per task-17)
- [ ] #2 Each profile is verified against the dogfood findings and carries verified_with from a real system
- [ ] #3 No profile depends on the Docker or Nix kinds, command actions or survey (task-37, task-38)
- [ ] #4 Dormant nix-shell and nix develop sessions keep their rc and TMPDIR dirs (container test), or nix-shell-rc and nix-shell-tmpdir do not ship in 0.1
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-68: open question, unchecked. The dry run found every nix-shell rc dir (/tmp/nix-shell-*-*, /tmp/nix-develop-*-*) eligible: 65,458 judged, no in_use, though live shells were running on the laptop. Nothing has them open and no environment names them, so the in-use check cannot see a live shell's rc dir. Is deleting a live nix shell's rc dir harmless (read once at startup?) or does a live shell re-read it? Check before nix-shell-rc ships (this task's AC #4 covers TMPDIR dirs, not rc dirs).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
