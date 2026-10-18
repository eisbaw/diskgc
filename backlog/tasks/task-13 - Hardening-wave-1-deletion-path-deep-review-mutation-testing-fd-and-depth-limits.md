---
id: TASK-13
title: >-
  Hardening wave 1: deletion path deep review, mutation testing, fd and depth
  limits
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-18 19:02'
labels:
  - wave-1
  - hardening
dependencies:
  - TASK-12
  - TASK-58
  - TASK-60
  - TASK-64
references:
  - docs/testing.md
  - docs/safety.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the wave-1 surfaces (host primitives, deletion engine, in-use checks, Cargo protocol) are now stable and are the hardest to walk back. Deep review (mped-architect plus a safety reviewer), cargo-mutants on the safety and executor modules (a surviving mutant in a safety check is a test bug), deep/huge tree limits, doc sync of docs/safety.md, plus every finding deferred during wave 1.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Deep review findings fixed or filed with an owner
- [ ] #2 docs/safety.md matches the implemented protocols (reviewed line by line against the code)
- [ ] #3 cargo-mutants (workspace tests) on diskgc-core and on diskgc-linux path resolution, mountinfo and /proc code: no surviving mutants in safety checks, or each justified; every safety check has a model test
- [ ] #4 Re-plan gate: if the race tests changed the Cargo protocol, task-9 and task-53 are re-reviewed under the deep gate and tasks 16, 22 and 33 re-checked; if the race tests cannot pass, cargo-target stays planned and the journey is rescoped
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: 'just mutants' is a placeholder (prints NOT RUN, exits 0). Replace it with the cargo-mutants invocation.

Dependencies changed for the 0.1 ensure MVP (2026-10-18): task-52 (CI) removed, because the human decided e2e-full runs locally for now (just e2e-full on the development machine is the evidence until CI exists). task-61 removed: it is split; its real-machine and safety items moved to task-64, which this task now depends on, and the rest is deferred to wave 5.

Filed from task-10's deep gate (Codex, mped, QA; file, do not fix in task-10): (H1) refusal messages name task IDs ('not supported ... (task-16)'): user-facing text should not cite backlog IDs once released; reword before 0.1. (H2) config lookup with a dangling symlink at the user path: read_config_file gets ENOENT and the search falls back to /etc, while a dangling user config is arguably 'exists but unreadable' (exit 2, no fallback); decide and test. (H3) deadline start point: Request.timeout is turned into a deadline when the executor starts, after startup, config and selection; the contract says 'for the whole call'; start it at process start or document. (H4) restrict Host::inherit_lock to the host lock (today any Locked can be made inheritable): e.g. a HostLock-only method or a sealed type. (H5) btrfs subvolumes: other_fs compares statx devices, and every subvolume has its own anonymous device although they share one pool's space: targets on a sibling subvolume are skipped other_fs; decide (statfs f_fsid?) and test on the model. (H6) a PATH with a symlink on the way (Fedora Silverblue /home -> /var/home) is refused as unsafe_path with the generic resolve message; say which component is a symlink and how to name the real path. (H7) switch CLI parsing from the hand-rolled parser in crates/diskgc/src/main.rs to lexopt (a reviewed production dependency, PRODUCTION_DEPENDENCIES) once the flag set grows (task-21).

Filed from task-10's deep gate round 2: (H8) docs/testing.md says the other binary tests stop at usage errors before the machine is read, but crates/diskgc/tests/cli.rs diskgc() isolates only DISKGC_CONFIG and XDG_CONFIG_HOME: tests without --config still look up /etc/diskgc/config.yaml on the host (a present, invalid file there changes their outcome); pass a config of the test's own everywhere, or document the dependency. (H9) the tally does no idle check, so a target that is not idle is locked (every Cargo lock of its profiles) and walked under the host lock before reclaim skips it as not_idle: a Cargo build starting in that window waits; consider an idle pre-filter from the tally's last build before locking (the protocol still rechecks under the locks).

(H10) From task-10 round 4 (mped nit 3): resolve every root before discovery walks any of them, so a misconfigured root (unsafe_path) excludes the tier before any other root's walk is spent; today cargo_target::discover walks roots outermost first and refuses an unsafe one when it reaches it.

From task-10 final round (filed at commit 783609f): H11 (QA) no container test runs a reclaiming ensure with a kept or not-owner target on the tallied path and asserts from the strace record that its .cargo-lock is never flocked (model-only today); cheap: add a .diskgc-keep target to ensure_exits_1_when_every_candidate_was_processed_below_min_free. H12 (mped) executor excluded() builds its message by stripping the Skip/ResolveError display prefix; carry the reason as data instead of re-parsing a string.

From task-11 (Cargo race tests, implementer, 2026-10-18): H13 race test 2 (cargo_race_2_a_build_started_during_deletion_waits_writing_nothing) proves 'a Cargo writes nothing into the profile directory before .cargo-lock' by comparing the harness's walk (inode, size, mtime, ctime) before the Cargo starts and while it waits; a file created and removed in between is invisible wherever the Cargo also created a lock file (the profile directory's times move anyway). A one-off strace -f measurement covers the three pinned toolchains (docs/safety.md, Lock files: what a Cargo writes before .cargo-lock). To catch it for a new toolchain on every run, have race test 2 run the waiting Cargo under its own strace of file-changing calls and fail on any call below the profile directory before the flock of .cargo-lock. H14 race test 8's late case (cargo run started while the driver, having judged the target eligible, is paused before its first lock) catches a scan kept from before the locks (Checker::forget removed under the locks) only while the pause is shorter than REUSE_LIMIT (500 ms), as it is on the development machine; on 1.90.0 the case often ends 'changed' instead (cargo run rewrites target/debug/<bin>.d) and then cannot catch it. A driver-only way to stretch the reuse window, or a model test pinning the forget, would make it deterministic (a_process_started_between_judging_and_locking_is_seen in the model covers the step today).

From task-11 round 2 (deep gate, 2026-10-18): H15 (mped S7) a test whose case cannot apply to a toolchain (no newer Cargo on the oldest stable, no .cargo-build-lock or split build directory on 1.90.0) logs 'not applicable' but is reported PASS; add an N/A outcome to the runner like TOOLCHAIN (exit code, summary column), or do not register such a NAME@TOOLCHAIN at all. Today: cargo_race_2_*'s cross-version rounds and cargo_race_2_split_build_dir_on_a_target_first_built_whole on 1.90.0, cargo_race_7's lock-order part and cargo_race_split_build_dir_is_never_reclaimed on 1.90.0. H16 (mped Q3, low) race test 6 on a shared target-dir from .cargo/config.toml (two projects' units in one profile, the detached rule): not run yet. H17 (optional) the compile-to-runner window of a Cargo whose target-dir points at another project's attached target (docs/safety.md In use, blind spot) could be reached with strace --inject=execve:delay_enter on the Cargo, so a race test can show what diskgc does there (expected: deletes, the documented limit).

From task-11 r2 review (mped NITs, commit 75c89c1): H18 the clean test declares the whole target a concurrent writer, so the oracle cannot judge diskgc's unlinks before the pause; assert unlinked(&run)[..k-1] fall in parts before the one in progress (part_of). H19 test-code duplication: inline // edit writes in the split and shared tests duplicate edit_main; the manifest needle string is duplicated between race 8 and late_run; M2 builds --pause-at=... strings only to split them again; the clean -p walk check is a weaker copy of race 2's (factor race 2's check out and reuse it). H20 oldest_stable ranks an unparseable version last via unwrap_or(u64::MAX); it should fail. H21 optional: whole build, then split-mode build, then reclaim, then split-mode build/run/test (Cargo re-linking fresh units into the target is unpinned).

From task-71 review round 1 (2026-10-18), on the task-11 follow-up: (mped) race test 8 still passes with only a WARNING when no late round is ever exercised on a toolchain (every round wrote into the profile and was changed, or the driver paused past REUSE_LIMIT): the scan-under-the-locks claim can go unexercised on a toolchain without failing. Decide whether that should fail, or count rounds across runs. (QA F4) race 2's clean case: 'ran to its end without waiting' rests on a single /proc/locks sample; a short wait between samples could be missed.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
