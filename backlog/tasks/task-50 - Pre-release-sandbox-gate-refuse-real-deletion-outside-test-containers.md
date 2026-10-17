---
id: TASK-50
title: 'Pre-release sandbox gate: refuse real deletion outside test containers'
status: To Do
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-17 08:41'
labels:
  - wave-1
  - irreversible
  - safety
dependencies:
  - TASK-2
references:
  - docs/safety.md#pre-release-sandbox-gate
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: from the first deleting code on, developers and AI agents could delete real data on their own machines with a work-in-progress build. Implement the gate in docs/safety.md#pre-release-sandbox-gate inside LinuxHost's deleting primitives (unlink, rmdir, the delete walk, and spawning prune/GC/command children), so every entry point is covered, including wave-1 test drivers: they refuse unless /.diskgc-test-sandbox and /run/.containerenv both exist. Compiled in by default; only the v1 release package (one build flag, set by the release task) removes it.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Calling a LinuxHost deleting primitive on a scratch directory outside a container returns the gate error and deletes nothing (test run outside the harness)
- [ ] #2 Non-deleting primitives (stat, read, walk without delete) work outside a container
- [ ] #3 The default build contains the gate; a test inspects the build configuration so the gate cannot be removed except by the release flag
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-2: diskgc_linux::sandbox already holds MARKER_PATH, CONTAINERENV_PATH and MARKER_CONTENT (include_bytes! of crates/diskgc-linux/src/sandbox-marker, which the flake copies byte for byte into the e2e base image) plus check() -> Result<(), NotInSandbox>. The e2e interlock (diskgc-e2e/src/interlock.rs) uses it. The gate must reuse it, not define the markers a second time. check_under(root) is private and unit-tested; expose it (pub(crate) or cfg(test)) if the gate's tests need a fake root. Pattern for 'refuses outside a container': the runner runs diskgc-e2e-canary on the host before every e2e run and requires exit 86.

Forward-carried from task-3: No deleting primitive exists yet. Add unlinkat/rmdir (and child spawning) as separate Host methods on LinuxHost and gate exactly those with diskgc_linux::sandbox::check(). The non-deleting primitives (openat2, statx, statvfs, flock, /proc readers) must keep working outside a container (AC #2). diskgc-linux's unit tests already exercise them on the dev machine read-only (a temp dir only for flock).

Forward-carried from task-14: the deleting operations are the trait diskgc_core::host::Delete (unlinkat so far), separate from Host. Gate LinuxHost's impl Delete; ModelHost (test-only, never in the binary) implements it without the gate since it deletes nothing real.

From mped's review of task-14: a gate refusal should be a distinct, non-errno HostError (e.g. errno None with a detail naming the sandbox gate and which marker is missing), so callers and tests can tell it apart from a kernel refusal.
<!-- SECTION:NOTES:END -->
