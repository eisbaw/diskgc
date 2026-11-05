---
id: TASK-345
title: >-
  the_daemon_probe_classifies_what_the_socket_does intermittent: 'refused' step
  gets Eof (listener fd inherited across a concurrent test's fork)
status: Done
assignee: []
created_date: '2026-11-05T01:19:00Z'
updated_date: '2026-11-05T02:39:00Z'
labels:
  - flaky
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
2026-11-05, nix develop -c just e2e on t342-stderr 976db895 (TASK-342 stacked on master 1da02f16), just check stage: diskgc-linux --lib host::tests::the_daemon_probe_classifies_what_the_socket_does panicked at host.rs (expect_err("refused")): 'refused: Eof'. Raw log: tmp/evidence-daemon-probe-flake/. Hypothesis (orchestrator, unconfirmed): after drop(silent) the test expects ECONNREFUSED, but another test in the same binary forks concurrently (e.g. a_process_execing_from_a_thread_in_a_loop_is_never_gone); a child between fork and exec holds a copy of the CLOEXEC listener fd, so the socket still listens, connect succeeds, and the exec closes it -> Eof. Do: confirm by reproduction (run the test alongside a forking loop), fix the test so the refused case cannot depend on fork timing (e.g. make the dead socket inode in a separate process that exits before the probe), no retry; prove the fix with the reproduction.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Merged 09224744. Cause confirmed: any concurrent spawn in the diskgc-linux test binary (not one specific test) holds a copy of the closed listener between fork and exec.
<!-- SECTION:NOTES:END -->
