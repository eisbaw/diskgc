---
id: TASK-51
title: e2e test driver and Host wrapper for controlled pause and kill points
status: To Do
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-17 09:36'
labels:
  - wave-1
  - testing
dependencies:
  - TASK-2
  - TASK-3
  - TASK-59
references:
  - docs/testing.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: wave-1 container tests need a diskgc process before the ensure command exists, and race tests 5 and 6 plus the replace-between-walk-and-unlink test must stop it at an exact call. Build the e2e-only test driver (a small binary over diskgc-core and LinuxHost that runs single operations on a fixture: walk, delete, in-use check, Cargo discovery and protocol) and the test-only Host wrapper from docs/testing.md#controlled-pause-and-kill-points (pause or SIGKILL at call k, driven by the harness). Neither is part of the release build.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The driver runs a walk and a delete on a container fixture, and the harness pauses it at a chosen Host call and resumes it, and kills it at a chosen call
- [ ] #2 The release build is verified not to contain the driver or the wrapper (a test inspects the release package)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-2: (1) Binaries reach containers through e2eUnderTest in flake.nix: its $out/bin is bind-mounted read-only at /opt/diskgc-e2e/bin. Add the driver there; code changes never rebuild the base image. (2) under_test::run_as_tester blocks until exit and records strace; pause/kill control needs a spawn variant. Control files the uid-1000 driver must read or write belong on the scratch mount: /run/diskgc-e2e is root-only. (3) Container root has CAP_SYS_PTRACE, so it can read /proc of the driver and kill it.

Forward-carried from task-59: (1) Judging is mandatory. The spawn/pause/kill variant of run_as_tester must enter its run in ctx.ledger (under_test::Ledger), record Run::started before spawning, and the test must pass the finished Run to Oracle::judge_runs. A run killed at call k is judged like any other; its strace record may end in '= ?' (Outcome::Unknown, counted as possibly effective). An oracle armed after a run started refuses it. (2) strace runs with --seccomp-bpf (under_test::STRACE_OPTIONS), which needs -f; a 'strace: ...seccomp...' stderr line fails the run, so keep -f in any new invocation. (3) Bite tests can be registered with suite::Expect::FailWith(text): the test passes only if it fails with that text. (4) Traced paths that do not match the before snapshot are classified and logged (oracle::Unmatched). A driver that renames or reopens directories will show SymlinkPrefix, NonCanonical or DeletedDirectory lines; those are diagnostics, and the snapshot diff is the verdict.

Forward-carried from task-3: diskgc-e2e now depends on diskgc-core directly as well as diskgc-linux. Keep diskgc-e2e/Cargo.lock in sync (cargo metadata --locked). A pausing or killing Host wrapper can be generic over H: Host and delegate to LinuxHost; every port call is a method, so call positions are countable. Probe pattern: diskgc-e2e/src/host_probe.rs runs LinuxHost as tester, prints indexed result lines, and host_suite.rs judges every run with the oracle.

From task-50: the test driver and the Host wrapper must delete only through LinuxHost's Delete methods, so through the sandbox gate. The source scan deleting_syscalls_only_behind_the_gate covers diskgc-linux only, so a raw unlinkat in the driver would not be caught: do not add one. The wrapper forwards Delete calls and adds pause/kill points; it must not mint permits.

Update from task-50's review: the scan (crates/diskgc-linux/src/source_scan.rs) now covers every production crate in crates/, but still not diskgc-e2e, where the test driver lives. A raw unlinkat in the driver would still not be caught; the driver must delete only through LinuxHost's Delete methods.
<!-- SECTION:NOTES:END -->
