---
id: TASK-59
title: >-
  Harness follow-ups before toolchains and race tests: mandatory oracle,
  seccomp-bpf strace, tmpfs for /run/diskgc and home
status: To Do
assignee: []
created_date: '2026-10-16 23:54'
labels:
  - wave-1
  - testing
dependencies:
  - TASK-2
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: findings from the task-2 review that would make later e2e results untrustworthy or too slow once Cargo builds run in the containers. (1) The oracle is opt-in: a traced run of diskgc that never calls Oracle::arm/judge passes silently; Ctx must fail any such run. (2) strace without --seccomp-bpf stops the traced process on every syscall; traced Cargo builds will blow the budget; use --seccomp-bpf. (3) Objects the harness creates outside a tmpfs live in the overlay upper layer where inode numbers are reused and birth time is unusable; mount tmpfs via Scratch on /run/diskgc (provision the lock there, as on a real host where /run is a tmpfs) and on /home/tester. (4) BTIME_UNSTABLE_FS_TYPES lists only overlay; detect fuse-overlayfs (podman info) and fail fast instead of producing false REPLACED. (5) Log strace removals that match no snapshot path (symlinked prefixes, .., '(deleted)' suffixes) instead of dropping them. (6) Skip read-only mounts (e.g. toolchains) in the whole-filesystem snapshot to keep it cheap.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A traced diskgc run without an oracle judgement fails the test (proved once)
- [ ] #2 strace runs with --seccomp-bpf; a traced cargo build in a container stays within a stated time bound
- [ ] #3 /run/diskgc and /home/tester are tmpfs in every container; the lock file is provisioned there
- [ ] #4 A non-native overlay storage driver makes the runner fail fast with a clear message
- [ ] #5 Unmatched strace removals are logged; read-only mounts are skipped by the snapshot
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
