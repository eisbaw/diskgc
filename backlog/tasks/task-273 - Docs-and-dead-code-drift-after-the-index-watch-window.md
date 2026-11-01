---
id: TASK-273
title: Docs and dead code drift after the index/watch window
status: To Do
assignee: []
created_date: '2026-10-31T03:39:00Z'
updated_date: '2026-11-01T16:40:00Z'
labels:
  - docs
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Full review 2026-10-31 on master 6f14dd7 (tmp/reviews/codex-full-6f14dd7.md; mped full review in the TASK-266-window notes); static reading, not reproduced. mped SHOULD 11-13, 15, 16 + NITs: testing.md names 8 tests that do not exist without an open marker (index and collapse; Index on the real kernel); architecture.md Implementation lists nixosModules.default and a module-evaluation check that flake.nix lacks (TASK-45); reference.md lists backend_unavailable without marking it reserved; stale module docs (diskgc-core lib.rs 'not yet wired (TASK-31)', executor.rs 'Task-31's index is separate', report NULL_FOR_NOW blaming task-28/29 instead of TASK-244, main.rs omits watch, tally/mod.rs 'watch (TASK-32)', diskgc-linux lib.rs module list); reference.md sandbox gate exit 3 vs code exit 2; watch JSON index.* bullets need '(null, TASK-244)'; architecture 'no shell scripts' claim false; nix_gc_* scenarios need '(TASK-36)'; dead Host::hostname, Budget::time_left; SourceFind::CargoTarget reserved for TASK-243; narrow watch internals to pub(crate). Add a cargo doc check with -D rustdoc::broken_intra_doc_links.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Periodic full review #2 on master 794e0e1 (2026-11-01; Codex tmp/reviews/codex-full2-794e0e1.md + mped), static reading, not reproduced. docs drift: testing.md names background_tally_does_not_load_the_system, sync_tally_is_useful_within_its_budget, snapshot_file_is_private_and_atomic, collapsed_index_reclaims_the_fixture, watch_and_ensure_share_a_filesystem (not existing, not marked open) and three renamed without 'as X' (discovery_dissolves_a_block_only_below_the_low_water_mark -> a_block_dissolves_only_when_its_members_fit_under_the_low_water_mark; ensure_skips_the_snapshot_when_it_would_cost_the_goal -> the_write_never_takes_the_reclaimed_filesystem_below_min_free; stored_cargo_refusal_is_retallied_without_locks: none). NITs: main.rs module doc (startup before config); watch/mod.rs two step-3 comments; dry_run_grouped_inner mismatch message; Writer::try_lock comment; inuse/scan.rs imports CAP_SYS_PTRACE from executor (move to kernel.rs); second-signal decision only in LinuxHost leaves_running (limit); MAPS_BY_INODE lacks shiftfs, aufs; release_verify does not check annotated tags; _not-yet recipe unused; reference.md watch exit table row 3 omits config read failure and unreadable capability set.

Periodic full review #3 on master fc33d98 (2026-11-01; Codex tmp/reviews/codex-full3.md + mped), static reading. docs drift: reference.md --min-free bullet (see the ensure-refuses task); watch exit-3 row incomplete (signal setup, config read, capability set, PID-namespace facts); watch --json refusal prints the full report shape, not the 3-field object; no per-build Commands caveat (0.2 refuses a reclaiming watch); backend_unavailable listed but nothing produces it; unknown_cause after three in a row inherits the standing cause, incomplete_process_list now only from mid-run changes; dead human-output paragraph on producers in another PID ns; -V undocumented. testing.md: index/collapse model + e2e tests, nix_gc_* scenarios and nix-* vectors (TASK-36), owner_pid_is_unknown_under_hidepid, virtiofs/9p vectors shown as current - mark planned. architecture.md: write guard wrong on loop devices (PATH's own loop-backed fs is guarded and written; unreadable loops case missing), Found ahead bound worded as a rule, TASK-292 one-lookup-per-directory undocumented, NixOS assertion wording; safety.md loop deleted-name step compares statx st_dev (say why), loop error list omits ELOOP/ENODEV/ENXIO; state.rs module doc per-user lock path. Pending markers: retarget the PRD loop/hidepid bullets, Weak-assumptions line, safety.md In use rule and the reference.md release-notes entry to TASK-296; 'TASK-117, open for Mark's confirmation' is stale (decided 2026-10-25); architecture.md 'Choices to confirm' has no task.
<!-- SECTION:NOTES:END -->
