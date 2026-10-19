---
id: TASK-46
title: >-
  Non-Nix run: e2e subset on a Debian image with the static binary, with and
  without Docker
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-19 04:49'
labels:
  - wave-5
  - deploy
  - testing
dependencies:
  - TASK-44
references:
  - docs/testing.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: nothing may silently assume Nix or a particular tool. Run the e2e subset in a container from a common non-Nix distribution image with the static binary, without Nix installed, and in a variant without Docker.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The fast e2e suite passes on the Debian image with the static binary, without Nix, and in a variant without Docker; missing tools yield missing skips, never errors
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-2: task-1 review item (d) (run on a non-Nix image in a Debian container test) was not done in task-2 and belongs here. Constraint: the e2e test binary refuses to run without /.diskgc-test-sandbox holding the marker constant, so a Debian variant needs a derived image carrying the marker, users tester/other (1000/1001), /run/diskgc/lock, strace and the mount points. One option is dockerTools.buildLayeredImage with fromImage = dockerTools.pullImage (debian, pinned digest), reusing e2eBaseImage's fakeRootCommands. Bind-mounting the marker at run time would weaken interlock layer 2, so do not. The binaries under test are static and bind-mounted, so they need nothing from the image.

The Debian part of this task (the e2e subset on a Debian image with the static binary, without Nix) was pulled forward into task-63 for the 0.1 ensure MVP (2026-10-18). The variant without Docker, and 'missing tools yield missing skips', stay here.

From task-63: the Debian image exists (flake.nix e2eDebianImage, just e2e-debian) with a per-image descriptor (diskgc-e2e/src/image.rs). The no-Docker variant can be a third descriptor; 'toolchains none' (no Cargo tests, no manifest read) is implemented and unit-tested but no image uses it yet. Extend PORTABLE_TESTS (suite.rs) rather than adding a second list.

CORRECTION (task-63 review): neither e2e image has Docker, so the Debian image already is the variant WITHOUT Docker. What is missing is a variant WITH Docker (dind), which needs the Docker kinds (task-35) first. The Debian image now runs every test but NIX_ONLY_TESTS (suite.rs), not a subset list; extend that deny list (with a reason per entry) rather than adding an allow list.
<!-- SECTION:NOTES:END -->
