---
id: TASK-44
title: >-
  Generic Linux deployment: systemd template unit, tmpfiles drop-in, example
  config
status: To Do
assignee: []
created_date: 2026-10-16 22:41
updated_date: '2026-10-31T05:13:00Z'
labels:
  - wave-5
  - deploy
dependencies:
  - TASK-32
  - TASK-119
references:
  - docs/architecture.md#deployment
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: most users are not on NixOS. Ship a systemd template unit for watch (user and system variants), the tmpfiles drop-in for the host lock, and docs/example.yaml consistent with docs/reference.md.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 systemd-tmpfiles --root=<scratch> --create creates the lock file as specified (container test)
- [x] #2 example.yaml passes check
- [x] #3 systemd-analyze verify passes on an instance of the template unit with the binary present
- [x] #4 Every --allow-root run (ensure and watch, all modes) exits 2 at start (cause safety) when CAP_SYS_PTRACE is missing from its effective set, read once from CapEff in /proc/self/status; safety.md Privileges states the rule; a container test runs root without that capability and expects the refusal (docs/architecture.md Deployment, Root; TASK-119 review B1)
- [ ] #5 Install steps in the README and release notes: systemd-tmpfiles --create for the host lock, then systemctl daemon-reload (or --user daemon-reload) before the first enable of a diskgc-watch@ instance; on NixOS a user instance starts only after the user's manager restarts (docs/architecture.md Deployment)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-2: the e2e base image currently bakes /run/diskgc (0755 root) and /run/diskgc/lock (0644 root) per the two tmpfiles lines in docs/safety.md (fakeRootCommands of e2eBaseImage in flake.nix). sandbox_trivial checks owner and mode. Once the shipped tmpfiles drop-in exists, apply it in the harness with systemd-tmpfiles --root=<scratch> --create (docs/testing.md, Shipped files) and drop the hand-written copy, so there is one source.

Pulled out into task-66 for the 0.1 ensure MVP (2026-10-18): the tmpfiles drop-in for /run/diskgc/lock and the example config (ACs #1 and #2 here, and the forward-carried harness note above). What stays here: the systemd template unit for watch (user and system variants, AC #3).

Orchestrator 2026-10-23 (design audit): now depends on TASK-119 (unit/module design).

Orchestrator 2026-10-24 (COMPASS round 2): dependency changed from TASK-43 to TASK-32 (the unit needs watch). The tmpfiles drop-in and example config were delivered by TASK-66; those ACs are satisfied there and must not be re-implemented.

Implementer 2026-10-31 (branch t44-units, b087673 + c797406): units in packaging/systemd/{user,system}/, shipped by the development package only (lib/systemd/{user,system}/; stdenv fixup moves lib/systemd/user to share/systemd/user and symlinks it back). NOT in release-01-candidate: 0.1 is the ensure MVP (PRD Scope), TASK-67 owns its contents. AC1: shipped_tmpfiles_and_example_work_in_container PASS (e2e-one, nix image). AC2: config-check-sandbox flake check builds. AC3: flake check deployment-units (system + --user verify, two instance names, packaged binary; system mode chrooted into a tmpfs root via unshare because the sandbox has no /run) + tools/unit_lint.py in just check. AC4: Host::own_cap_effective, executor::privileges refuses every --allow-root run without CAP_SYS_PTRACE (also non-root, also unreadable CapEff); model test, 3 vectors, watch model test, container test allow_root_without_cap_sys_ptrace_is_refused_in_every_mode (nix and debian images). AC5 only partly: install steps live in docs/reference.md 'Watch as a systemd service' (dev package); packaging/README.md and the 0.1 release notes are the 0.1 package's and do not mention watch units. Open: decide where watch install steps go once a release ships watch (TASK-47/49).

Review round 1 (Codex + mped on d326603) fixed in d864c0d: --allow-root requires euid 0; CAP_SYS_PTRACE withholds SetUid/Privileged/NonDumpable excuses (gap predates TASK-44, also in master's release-01-candidate by code reading); unreadable CapEff is host/exit 3; Host::own_cap_effective dropped for proc_status(own_pid); --user verify unchrooted; reference.md states a reclaiming instance needs a release package with watch.
<!-- SECTION:NOTES:END -->
