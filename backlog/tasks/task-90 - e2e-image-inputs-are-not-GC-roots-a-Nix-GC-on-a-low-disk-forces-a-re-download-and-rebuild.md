---
id: TASK-90
title: >-
  e2e image inputs are not GC roots: a Nix GC on a low disk forces a re-download
  and rebuild
status: Done
assignee: []
created_date: '2026-10-19 04:20'
updated_date: '2026-10-19 08:11'
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
- [x] #1 Either the e2e image inputs are GC roots created by the recipes, with their disk cost stated in docs/testing.md (Images), or docs/testing.md states that a GC forces a re-download; the peak disk use of a first Debian image load is measured and stated
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done (2026-10-19, task-63 follow-up): the container recipes build each runner to an out-link under target/gcroots/ (e2e-runner, e2e-runner-debian; justfile _runner) and run it from there, and root the dev shell (`nix develop --profile target/gcroots/dev`, history wiped to the current generation); all three are registered GC roots (nix-store --gc --print-roots), each build replaces its link. The runners' closures hold the image inputs (the pinned Debian image tarball, the toolchains, the built image layers). Costs stated in docs/testing.md (Images, GC roots): closures 3.0 GB (Nix runner), 1.2 GB (Debian runner), 1.5 GB (dev shell), partly shared. Peak disk use of one Debian image load, measured apart from any build (runner out-link prebuilt, image removed from Podman, free space sampled every 100 ms): 2.80 GB at the peak, 0.95 GB kept (the image, 929 MB in Podman), 1.86 GB only during the load; load 5.3 s. One sample on a machine with other activity. Not measured: the Nix image's load (2.84 GB image; it would need a forced reload). The ~4.8 GB unexplained drop of the first load stays unattributed beyond this.

Round 2 (task-63 follow-up review): the roots moved to .gcroots/ at the repo root (gitignored; cargo clean and rm -rf target no longer drop them); the dev shell root is now an out-link of devShells.x86_64-linux.default.inputDerivation (1.54 GB closure), the same shape as the runners (the --profile form failed on a fresh clone: no parent dir, and kept generations). Verified from a fresh git clone with no target/ or .gcroots/: _gcroot-dev and a runner build and run succeed. Rooted closures 4.4 GB together; Podman's image copies (2.84 + 0.93 GB) are extra. Dropping: just gcroots-drop, then nix store gc and podman rmi (docs/testing.md, Images, GC roots).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
