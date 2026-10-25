---
id: TASK-166
title: >-
  In-use scan under RLIMIT_NOFILE 1024 on a busy host: every candidate quietly
  in_use (pidfd_open EMFILE); binary never raises its soft fd limit
status: To Do
assignee: []
created_date: '2026-10-25 01:12'
updated_date: '2026-10-25 05:33'
labels:
  - hardening
  - limits
  - shipping
dependencies: []
references:
  - docs/safety.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-13 async QA (fd limits), measured on the host with the read-only dry run (tmp/target/debug/diskgc built 2026-10-25 01:06 from master near b009e59; host had 1911 processes, NixOS soft limit 524288):
  (ulimit -n N; diskgc ensure /tmp --dry-run --timeout 40s --min-free 900G --select profile:pytest-tmp)
  N=40, 512, 1024: every candidate printed: skip in_use: ... a process identity could not be checked: pidfd_open <pid>: EMFILE (24)
  N=2048: no in_use line in the 40 s window (inconclusive on the threshold).
Cause: a scan holds one pidfd per listed process incarnation (docs/safety.md In use, limit 4) plus per-thread task fds; past RLIMIT_NOFILE the scan is unsettled and blocks every candidate. The product never calls setrlimit (only test helpers do).

User impact: stock Ubuntu/Debian/Fedora give interactive shells and most units a soft NOFILE of 1024 (hard 524288). A dev laptop or CI host with ~1000+ processes (this host: 600-1900) gets zero reclaim, reported as in_use, no error, exit 1. safety.md documents it as an availability limit ("A low file-descriptor limit can therefore prevent reclamation on a busy host"), but it contradicts the walk's own principle in Filesystem candidates: "EMFILE ... is an error of the run ... a low fd limit fails loudly instead of quietly skipping every remaining candidate" (same class, opposite handling).

Proposed: (1) at startup raise the soft RLIMIT_NOFILE to the hard limit (standard practice, one setrlimit, no safety cost); (2) treat EMFILE/ENFILE from pidfd_open and /proc reads in the scan as an error of the run (exit 3, loud), consistent with the walk; (3) a container test running ensure under ulimit -n 64 with >64 processes asserting the loud outcome.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Binary raises soft RLIMIT_NOFILE to the hard limit at startup (or a documented reason not to)
- [x] #2 EMFILE/ENFILE during the in-use scan is an error of the run, not a per-candidate in_use skip; docs/safety.md says one rule for both
- [x] #3 Container test: ensure under a low fd limit and more processes than fds exits loudly; nothing deleted
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 t166 d84cba8 (not merged): measured fd need = one pidfd per visible process held for the whole scan + transient reader fds (peak 2402 at ~2250 host processes; 3694 at ~3500). Pin kept (replacing it weakens safety.md In use limit 4). Change: nofile::raise_to_hard at startup (children get inherited soft back in pre_exec); EMFILE/ENFILE anywhere in gather -> HostError with advice -> Stop::Failed -> exit 3 cause host. Tests: model vectors per scan reader + EIO control + deletion-step stop; nofile re-exec unit test; container test inuse_running_out_of_descriptors_fails_the_run_unless_the_soft_limit_can_rise (e2e-one PASS). Walk (container t166, 1897 procs, idle pytest-tmp, tmp/walks/2026-10-25-t166/4*-idle.txt): before soft1024 -> 4x in_use pidfd_open EMFILE exit 0; after soft1024 -> scan settles (producer_unknown: not initial pid ns) exit 0; after hard1024 -> exit 3 EMFILE + advice. Host read-only dry runs (host-walk.txt): after soft1024 peak 3694 fds, scan runs; after hard1024 exit 3 loud. Gates: just check exit 0 (1124 passed/0 failed/1 ignored) x2; e2e-clippy, e2e-tests exit 0. mped-architect: ship, no blockers.
<!-- SECTION:NOTES:END -->
