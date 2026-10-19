---
id: TASK-90
title: >-
  e2e image inputs are not GC roots: a Nix GC on a low disk forces a re-download
  and rebuild
status: To Do
assignee: []
created_date: '2026-10-19 04:20'
updated_date: '2026-10-19 04:39'
labels:
  - testing
  - wave-1
dependencies: []
references:
  - flake.nix
  - justfile
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while implementing task-63 (2026-10-19). Free space on the development machine fell from 7.6 GB to 1.9 GB during the first Debian run, and an automatic Nix GC (min-free) then removed unrooted store paths, among them the pinned Debian rust image (0.9 GB, fixed-output, re-downloaded from Docker Hub on the next build) and the static strace (rebuilt, with about 370 MB of musl build tools fetched again), plus the dev shell's inputs. The runner needs an image's inputs only when its tag is not loaded in Podman, but any flake rebuild of the runner pulls them in again.

Options: `nix build --out-link` GC roots for the image streams in the e2e recipes (costs the disk permanently), or document that a GC means re-downloads. Also measure what took the remaining ~4.8 GB during that load (podman load spooling the stream to a temp file is suspected, not measured).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Either the e2e image inputs are GC roots created by the recipes, with their disk cost stated in docs/testing.md (Images), or docs/testing.md states that a GC forces a re-download; the peak disk use of a first Debian image load is measured and stated
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
