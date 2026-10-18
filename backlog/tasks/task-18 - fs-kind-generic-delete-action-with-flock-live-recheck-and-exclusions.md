---
id: TASK-18
title: 'fs kind: generic delete action with flock, live recheck and exclusions'
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-18 13:16'
labels:
  - wave-2
  - irreversible
  - safety
dependencies:
  - TASK-16
references:
  - docs/safety.md#filesystem-candidates
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: most cruft (/tmp leftovers, caches) is plain files. Wire the fs kind's delete action through the engine and in-use checks: flock the candidate, eligibility walk and in-use under the lock, identity-checked delete; PrivateTmp entries and network filesystems excluded; tmpfs matching behaviour.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The 100k-entry /tmp fixture: every eligible pytest-of-* and nix-shell rc dir is deleted, nothing else (deletion oracle)
- [ ] #2 A file created mid-delete yields an ENOTEMPTY error for that candidate, never a silent retry
- [ ] #3 systemd-private-* entries are never candidates
- [ ] #4 Tests drive the engine with user-profile fixtures (no built-in names before wave 4)
- [ ] #5 A candidate on a network filesystem is never deleted (model test); a socket or FIFO is never deleted (container test)
- [ ] #6 A dead FUSE mount in /tmp does not hang the scan (container test with /dev/fuse and a FUSE server that never answers)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-5 (deletion engine): the fs kind wires engine::walk -> (in-use, task-6) -> engine::delete under the candidate's flock. Not in the engine, still this task's: mountinfo first (a dead FUSE mount point inside a candidate would block the walk's statx; the walk itself only detects mounts by stx_mnt_id/EXDEV), systemd-private-*, network filesystems, .diskgc-keep in directories between the profile root and the candidate (engine::KEEP_MARKER; the walk only looks inside), the idle filter from WalkRecord::newest_change() (None = some time unreported: not idle), protect/negation. The flock fd should be checked against WalkRecord::root().identity.id. make_writable maps to DeleteOptions::make_writable. A file created mid-delete gives End::Failed with ENOTEMPTY (AC #2 here). Sockets/FIFOs are refused by the walk (contains_ipc; container test engine_skips_sockets_and_fifos exists).

From task-68 (read-only spike): crates/diskgc-core/src/fs_kind/plan.rs is the read-only half of this task's fs kind: segment-wise discovery (mountinfo first: a mount point is never statted; systemd-private-* never a candidate nor entered; outermost root first; identity dedupe), then per candidate the source's has/has_not, other_fs, keep markers between root and candidate, the engine walk, identity, idle (WalkRecord::newest_change) and the in-use check. It judges each candidate while its parent is open (one fd per pattern level), which a reclaim cannot reuse as is (it needs flock + walk + check + delete under the lock, with a scan <= 1 s old). executor::ensure refuses a reclaiming run over kind fs as a usage error before anything else (model test a_reclaiming_ensure_over_fs_candidates_is_refused_before_anything): this task lifts that refusal. Measured on the laptop: at nice 19 / ionice idle under load 10 to 90, ~55 candidates/s judged; nix-shell-rc (105k candidates, ~3 KiB each) did not finish a 20 min dry run; contains_ipc kept 158 pytest dirs and 199 nix-shell.* dirs (stale test FIFOs and sockets under the default policy). The fs size estimate counts hard links per name (claude-code-tmp: 2,794 MiB vs du 2,599 MiB); the cargo kind dedupes by identity, the fs kind should too. See task-70 for scan cost.

From task-68 final review (commit 19a47e3): (1) Codex F4 residual: fs_kind/plan.rs Walk::visit can still finish past the deadline with exit 0 when the LAST matching entry's statx crosses it and its type fails find.type, when a listing returns TooMany or a skippable error (bypasses the post-listing check), and between has and has_not in source_where::holds. Check the deadline before predicate I/O and once before discovery reports success; add tests for each exit. (2) mped N1: Walk::candidate repeats the identity/seen-set/duplicate logic in the new mount block; do identity+dedup once at the top, then mounts, then where, then judgement, when splitting discover from judge. (3) mped N4: Planned.private_tmp doc says it counts names a pattern matched; it now counts roots too. (4) QA: the PrivateTmp root check (F3) is by path name only; a bind mount of a systemd-private-* dir elsewhere is not caught (symlinked roots are already refused). State it in safety.md or check by identity.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
