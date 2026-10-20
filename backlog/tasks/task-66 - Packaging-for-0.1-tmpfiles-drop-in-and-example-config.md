---
id: TASK-66
title: 'Packaging for 0.1: tmpfiles drop-in and example config'
status: In Progress
assignee: []
created_date: '2026-10-17 23:55'
updated_date: '2026-10-20 11:36'
labels:
  - wave-2
  - deployment
dependencies:
  - TASK-15
references:
  - docs/safety.md
  - docs/architecture.md#deployment
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: 0.1 ships as a static binary with a tmpfiles drop-in and an example config (PRD Scope, Releases). Pulled out of task-44: the tmpfiles lines for /run/diskgc and /run/diskgc/lock (docs/safety.md, Host lock) and docs/example.yaml only; the systemd watch unit stays in task-44.

Carried from task-44's note: the e2e base image bakes /run/diskgc (0755 root) and /run/diskgc/lock (0644 root) by hand (fakeRootCommands of e2eBaseImage in flake.nix). Once the shipped drop-in exists, apply it in the harness with systemd-tmpfiles --root=<scratch> --create (docs/testing.md, Shipped files) and drop the hand-written copy, so there is one source.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The drop-in creates /run/diskgc/lock as PRD.md and docs/safety.md (Host lock) describe (systemd-tmpfiles --root=<scratch> --create, container test)
- [x] #2 The example config passes diskgc check
- [ ] #3 Both are installed by the release package
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Ship one tmpfiles.d source with the static package and example config; exercise systemd-tmpfiles --root in a disposable test container and verify package artifacts and config check. Keep release gate enabled.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-12 round 2: ship docs/ci-pre-job.sh as an example (e.g. share/doc/diskgc/examples/) or state that it is docs-only. The lock path and the two tmpfiles lines exist in three copies today (the hook, docs/safety.md Host lock, the flake's e2e image and harness comments); the shipped tmpfiles drop-in should become the single source the others are checked against.

2026-10-20 partial packaging implementation in disposable worktree: the Nix diskgc package now installs packaging/diskgc.conf under lib/tmpfiles.d and docs/example.yaml under share/doc/diskgc/examples. New pinned-systemd disposable-container E2E applies the packaged rule under a scratch --root and checks root-owned 0755 directory plus root-owned 0644 lock; it also runs diskgc check on the packaged example. Focused E2E passed 1/1. The generic Nix and Debian harness still provisions each per-test lock by hand (Debian image has no systemd-tmpfiles), so the single-source harness migration in this task remains open. The release-without-sandbox-gate artifact does not exist yet (TASK67); do not mark AC3/DoD complete based only on the development package. No host diskgc or real host path tested.

2026-10-20 follow-up: Nix and Debian harnesses apply shipped packaging/diskgc.conf with real systemd-tmpfiles for every test, and both images run the packaged-rule --root scratch proof. Debian pins standalone tmpfiles 257.13-1~deb13u1 by SHA-256; Nix image carries pinned systemd. Focused container tests passed: Nix packaging 1/1, Debian packaging 1/1, Nix sandbox_trivial 1/1 after fixing missing Nix image tool closure. Alpine Docker dind still creates its lock directly; TASK66 remains In Progress. Ungated release package remains TASK67. No host diskgc or real project path tested.

2026-10-20 Alpine dind packaging probe: the pinned docker:29.8.2-dind base is Alpine 3.24.2; its v3.24 community repo has sd-tools 0.99.0-r3. In a disposable container, apk-installed /usr/bin/sd-tmpfiles applied the shipped packaging/diskgc.conf to a scratch --root and produced root-owned 0755 /run/diskgc and root-owned 0644 lock. The scratch root required etc/passwd and etc/group (otherwise sd-tmpfiles failed "Failed to find nobody uid"). The APK depends on libacl.so.1 and libcap.so.2; package plus libraries must be pinned and baked into the image before deleting its hand-created lock. No diskgc ran on the host.

Pinned-Alpine candidate inputs (official v3.24 CDN, SHA-256 of downloaded APK): main/acl-libs-2.3.2-r1.apk 3f02851d586c25d97ba8b5b845615aacc209ae7eaeb6f7843eca76d85da2249d; main/libcap2-2.78-r0.apk 42886a7b4bcde26347c712925c8dc7db202b6029ef9b8c7ee81577ab7bdd8fad; community/sd-tools-0.99.0-r3.apk 93cfc77c9c272f7d0d8e7e3879bd221770c3e3f04490cc7fb8080d2cbf5110f3. Extracting these three into a fresh Alpine dind container and using LD_LIBRARY_PATH also passed the --root scratch metadata probe. This is a packaging lead, not a completed Nix build or dind E2E.

2026-10-20 Alpine dind follow-up in disposable worktree: dind now carries SHA-256-pinned Alpine 3.24 sd-tools 0.99.0 plus acl-libs/libcap2, and every harness image requires a real tmpfiles path; the hand-created dind host lock is gone. A Docker-only scratch --root test applies the packaged rule, verifies root-owned 0755 directory and 0644 lock, and checks the packaged example. Focused proofs passed: Docker packaged rule 1/1, native Docker smoke 1/1, Nix packaged rule 1/1, Debian packaged rule 1/1; nix develop -c just check passed. The sd-tools 0.99.0 binary SIGSEGVs when --root scans a scratch root without an explicit rule file, reproduced in a disposable dind shell even with expected config directories and passwd/group. The test therefore names the staged shipped rule explicitly; it proves rule content and per-test lock provisioning, not automatic Alpine tmpfiles.d discovery. AC1/AC2 checked on container evidence; AC3/DoD remain open until TASK67 produces and verifies the ungated release package. No diskgc invocation on developer host or real project paths.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
