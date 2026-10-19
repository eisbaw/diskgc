---
id: TASK-63
title: Debian / static-binary e2e variant (non-Nix image)
status: Done
assignee: []
created_date: '2026-10-17 23:54'
updated_date: '2026-10-19 04:39'
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
- [x] #1 A Debian base image variant builds in the flake and runs a subset of the e2e suite with the static musl binary
- [x] #2 The harness opts out of the /nix/store bind mount and the toolchain manifest for that image (per-image opt-out)
- [x] #3 The CI-operator journey from task-12 runs on it
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-12 round 2: run the example pre-job hook (docs/ci-pre-job.sh, as the CI operator journey does) under dash, Debian/Ubuntu's /bin/sh, not only busybox sh; it uses mktemp, sed -E, grep -o and $(( )).

Implemented (2026-10-19): Debian image `localhost/diskgc-e2e-debian` (flake.nix e2eDebianImage, streamLayeredImage fromImage = dockerTools.pullImage of library/rust:1.99.1-slim-trixie pinned by digest sha256:4df2ae8c…13d and its hash). Shared image commands (e2eImageCommon: marker baked in, users, overlay file, mount points, descriptor) for both images. Cargo: the image's own rustup toolchain (glibc-dynamic, non-Nix), real binaries named in the manifest as debian-1.99.1, Debian gcc as linker; strace: static musl build of 7.2 copied as a plain file; no /nix/store in the container. Per-image opt-out: descriptor /.diskgc-test-image (diskgc-e2e/src/image.rs: read-only dirs, toolchains manifest|none, tests all|portable), required on every image; the Nix image says read-only /nix/store, toolchains manifest, tests all. Subset: PORTABLE_TESTS in suite.rs (21 tests incl. the journey, whose hook runs under dash). Recipes: just e2e-debian; e2e-full ends with the Debian run.
AC #1, #2, #3 met. Bites: Debian image without the sandbox marker: the runner stops at the listing (exit 86, INTERLOCK), the canary in that image exits 86; Debian descriptor without `read-only /usr`: snapshot_skips_read_only_mounts red (the toolchain is not below a read-only mount).
Limits: `toolchains none` is exercised by unit tests only (no image uses it yet; task-46's no-Docker variant or a Nix-daemon image will); Debian's own rustc package (1.85 in Debian 13) is not tested; the race tests run on the Nix image only.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
