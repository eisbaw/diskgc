---
id: TASK-66
title: 'Packaging for 0.1: tmpfiles drop-in and example config'
status: In Progress
assignee: []
created_date: '2026-10-17 23:55'
updated_date: '2026-10-20 10:43'
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
- [ ] #1 The drop-in creates /run/diskgc/lock as PRD.md and docs/safety.md (Host lock) describe (systemd-tmpfiles --root=<scratch> --create, container test)
- [ ] #2 The example config passes diskgc check
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
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
