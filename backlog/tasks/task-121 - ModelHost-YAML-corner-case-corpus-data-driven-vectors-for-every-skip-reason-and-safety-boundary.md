---
id: TASK-121
title: >-
  ModelHost YAML corner-case corpus: data-driven vectors for every skip reason
  and safety boundary
status: To Do
assignee: []
created_date: '2026-10-23 16:29'
updated_date: '2026-10-23 17:15'
labels:
  - verification
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark (2026-10-23): we should have many YAML mockfs cases, including corner cases. Today there are 36 checked-in YAML host models: 30 profile vectors (one per built-in source, almost all a single positive match case; only 3 include processes, 1 includes locks), 3 regressions with ad-hoc shapes, 1 sketch, 1 docker binding, 1 modelgen fixture. The safety corner cases (symlink, mount crossing, mixed owners, .diskgc-keep, checkout guard, sockets/FIFO policy, pid reuse, rename-between-check-and-unlink, deadlines, protect overlap, negation, roots binding, owners under --allow-root) are covered only by ~480 Rust test functions that build ModelHost programmatically - not as reviewable data.

Build: (1) one generic vector runner (profile_vector-style expect block: plan, reclaim_exit, actions, removed, skipped reasons, preserved) that runs every YAML under a corpus directory, usable with user_profiles/config/tiers, not only one built-in; (2) a corpus with at least one vector per skip reason in docs/reference.md and per line of PRD "What bad looks like" / safety.md eligibility walk; (3) per built-in source at least: match, live producer (in use), protected or negated, nested guard (.diskgc-keep or checkout); (4) a completeness test that fails when a skip reason or built-in source has no vector; (5) each vector proven to bite (mutation: documented in notes which check it kills). Keep the YAML schema as-is unless a corner case cannot be expressed; extensions go through docs/testing.md.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A generic YAML vector runner exists and every file in the corpus directory runs under just check
- [ ] #2 Every skip reason in docs/reference.md has at least one vector; a completeness test enforces it
- [ ] #3 Every built-in source has match + in-use + protected/negated + nested-guard vectors; a completeness test enforces it
- [ ] #4 Every PRD 'What bad looks like' line for diskgc's own actions has a vector that would fail if the guard were removed (bite evidence in notes)
- [ ] #5 docs/testing.md describes the corpus layout and how to add a vector
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from TASK-64 (2026-10-23): new YAML cases to fold into the corpus runner: profiles/python-pycache-fork-storm-unsettled.yaml and profiles/python-pycache-fork-burst-settles.yaml (in-use liveness; bites M1/M2/M3' recorded in TASK-64 notes) and hostlock/held-by-a-remaining-reference.yaml, hostlock/released-by-the-last-reference.yaml (run by tests/ensure.rs with a 2 s timeout because the host-model YAML has no request block; a generic runner needs request fields: select, roots, min_free, timeout). Gotcha: a modelled process with no fds is unreadable and blocks every candidate.

From TASK-112 (2026-10-23, branch task112-design): docs/testing.md 'Producer liveness vectors (planned)' specifies ~70 named profile vectors for liveness contracts (Owner PID / nix-shell-rc, pytest sessions, node/python project dependency directories), plus the profile_vector and host-model extensions they need (expect.plan/skipped[].evidence; pid_namespace, proc_hidepid, processes[].nspid/zombie/cmdline/view, tree atime, mount noatime). They are this corpus's liveness part: implement the extensions once, shared with TASK-112's implementation, and do not duplicate the cases.
<!-- SECTION:NOTES:END -->
