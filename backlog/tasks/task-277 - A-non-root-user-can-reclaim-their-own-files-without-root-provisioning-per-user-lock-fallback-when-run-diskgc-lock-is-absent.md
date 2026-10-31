---
id: TASK-277
title: >-
  A non-root user can reclaim their own files without root provisioning:
  per-user lock fallback when /run/diskgc/lock is absent
status: Done
assignee: []
created_date: '2026-10-31T17:03:00Z'
updated_date: '2026-10-31T19:16:00Z'
labels:
  - irreversible
  - safety
  - ux
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark's requirement (2026-10-31, while dogfooding 0.1.0 on the host): 'it shall be possible for a non-root user to run diskgc ... any user shall be able to garbage collect his own files'. Today ensure/watch exit 2 (provisioning) when /run/diskgc/lock is missing, and creating it needs root. Design: (1) a provisioned host lock is used as today (host-wide serialisation); (2) a non-root run without it uses a per-user lock created by diskgc: $XDG_RUNTIME_DIR/diskgc/lock (dir 0700, file 0600, O_NOFOLLOW, owner and mode checked), else the state directory; one log line names the lock and says reclaims by other users are not serialised with this one; (3) root / --allow-root / system services still need the provisioned host lock. Safe because the lock only serialises reclaims; every safety check is per candidate and the Cargo protocol uses Cargo's own locks; the cost (documented limit): two users reclaiming at once on one filesystem may free more than either needed. watch's tally pause and the snapshot ordering use the resolved lock. Creating the lock file is a state write (TASK-30's ungated state op pattern, exact source-scan counts). Evidence: dogfood run on the host, 0.1.0 release binary: exit 2 provisioning, nothing deleted (2026-10-31).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implementer (branch t-userlock, commits cc6cf22 1a7533a ba0923f 168d296): per-user lock fallback implemented (core hostlock open_lock/readiness/prepare, Host::user_lock, LinuxHost state.rs open_user_lock_ungated, ModelHost runtime_dir + user_lock), docs (safety.md Host lock, reference.md, architecture.md, PRD, packaging README), system template AssertPathExists=/run/diskgc/lock with a unit-lint rule. Gates: just check exit 0 (1385 tests); e2e harness unit tests (flake e2e-tests) 109 passed; deployment-units flake check exit 0. NOT RUN: container tests (14.9 GiB free < 15 GiB rule), including the new ensure_as_a_user_without_the_host_lock_reclaims_under_a_per_user_lock and the changed root/goal-met/dry-run/CI-journey ones. Host dry-run walk as uid 1000 shows the note without sudo and makes nothing. Open: review round (mped + Codex); per-user lock path depends on XDG_RUNTIME_DIR (session vs cron resolve different locks).

Review round 1 (Codex + mped on c0d0352) addressed in f02aa45 and 53f2338: environment-free per-user lock place (/run/user/<euid> if the user's 0700 dir, else account home's .local/state/diskgc; $HOME only without an account home), O_PATH inspection (foreign objects are provisioning, not EACCES), unlocked fallback with one warning for environmental failures (no place, ENOSPC/EDQUOT/EROFS/EACCES/EPERM/ENOTDIR), other host failures exit 3 host, probe and --tally-only make nothing, docs single-homed in safety.md#host-lock. Gates: just check exit 0 (1393), e2e-tests 109 passed; container (e2e-one, all PASS): the 4 earlier tests, ensure_as_a_user_refuses_another_users_per_user_lock, ensure_as_a_user_reclaims_unlocked_when_the_per_user_lock_cannot_be_made, ensure_dry_run_plans_real_targets_and_deletes_nothing@1.99.1, journey_ci_operator_frees_space_with_ensure@1.99.1.

2026-10-31: merged (ff) after one review round (mped + Codex). Lock path: /run/user/<euid>/diskgc/lock when that runtime dir is owned and 0700, else <passwd home>/.local/state/diskgc/lock; env vars ignored. Unsafe object -> exit 2 provisioning; environmental failure -> reclaim unlocked with a warning; other failures -> exit 3 host (contract change from 2 usage). 0.1.0 still requires the provisioned host lock: backport is part of the 0.1.x / 0.2 decision (TASK-276).
<!-- SECTION:NOTES:END -->
