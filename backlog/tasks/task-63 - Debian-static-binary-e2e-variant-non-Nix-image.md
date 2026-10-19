---
id: TASK-63
title: Debian / static-binary e2e variant (non-Nix image)
status: To Do
assignee: []
created_date: '2026-10-17 23:54'
updated_date: '2026-10-19 02:54'
labels:
  - wave-1
  - testing
dependencies:
  - TASK-12
references:
  - docs/testing.md
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: 0.1 ships a static binary for machines without Nix (PRD Scope, Releases), and nothing may silently assume Nix. Pulled forward from later tasks for the 0.1 ensure MVP:
- task-12 AC #3: the CI-operator journey also runs on a Debian image with the static binary (removed from task-12).
- task-60 H2: Ctx::setup always bind-mounts /nix/store read-only, and the test listing requires the toolchain manifest (/opt/cargo-under-test/toolchains); both break a non-Nix image, so each image needs a per-image opt-out.
- task-46, the Debian part: the e2e subset on a Debian image with the static binary, without Nix. The variant without Docker stays in task-46.

Constraints carried on task-46: the image must carry the sandbox marker in the image itself (never bind-mounted at run time, which would weaken interlock layer 2), users tester/other (1000/1001), /run/diskgc/lock, strace and the mount points. One option is dockerTools.buildLayeredImage from a digest-pinned debian image, reusing e2eBaseImage's fakeRootCommands.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A Debian base image variant builds in the flake and runs a subset of the e2e suite with the static musl binary
- [ ] #2 The harness opts out of the /nix/store bind mount and the toolchain manifest for that image (per-image opt-out)
- [ ] #3 The CI-operator journey from task-12 runs on it
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-12 round 2: run the example pre-job hook (docs/ci-pre-job.sh, as the CI operator journey does) under dash, Debian/Ubuntu's /bin/sh, not only busybox sh; it uses mktemp, sed -E, grep -o and $(( )).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
