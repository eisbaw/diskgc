---
id: TASK-13
title: >-
  Hardening wave 1: deletion path deep review, mutation testing, fd and depth
  limits
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-25 08:38'
labels:
  - wave-1
  - hardening
dependencies:
  - TASK-12
  - TASK-58
  - TASK-64
references:
  - docs/testing.md
  - docs/safety.md
priority: high
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

From the task-63 follow-up review (commit 19fc56e): H22 race 7's has_build_lock branches and the blocked-build lock check in cargo_suite.rs accept a missing .cargo-build-lock from ANY toolchain; tie the absence to the measured release (1.90.0), as IGNORES_BUILD_DIR does for the build dir, so a regression cannot pass vacuously. H23 the NIX_ONLY_TESTS lookup (split('@') + list search) is written in both suite.rs::run and Image::runs; one Image::excluded() returning the reason would remove the copy. H24 testing.md suggests 'nix store gc' without saying it is machine-wide; justfile gcroots-drop/e2e-one comments read backwards; the justfile header's 'only call Nix, Cargo and Podman' now has rm. H25 .gcroots does not keep the flake checks' vendored crates, so after a GC the first gate re-fetches them (documented scope is image inputs + dev shell).

Forward-carried from TASK-64 (2026-10-23): a flock (or a bound Unix socket) is released when the LAST reference to its open file goes, which can be after close()/exit returned: any /proc/<pid>/fd reader (an in-use scan, diskgc's own included, lsof) holds a transient reference for the length of its syscall (measured up to 0.5 ms on 7.2; about 1 in 60 immediate retries busy under four readers). Never assert a release with one attempt right after a close; wait with a bound (RELEASE_BOUND, docs/testing.md Negative feedback). A held-lock assertion needs no wait. Mutation testing: the unsettled-scan mutants M1 (Checker::reusable_at reuse), M2 (Checker::rescan no retry) and M3' (Scan::blocked ignoring unsettled) are killed by the profile vectors python-pycache-fork-*; Scan::blocks_all without unsettled is an equivalent mutant (reusable_at returns first) and will survive cargo-mutants.

Correction to the TASK-64 forward-carry above (2026-10-23, measured): which /proc reads hold a file reference depends on the kernel (fs/proc/fd.c): reading /proc/<pid>/fdinfo on every supported kernel, statting or reading an fd link from 5.11, listing the fd directory from 6.7. diskgc's scan lists and stats fd links, so it can delay another program's lock release only from 5.11 on; on 5.8.16 those reads gave no late release in about 1.7 million attempts. A test still waits with RELEASE_BOUND on every kernel (other readers, e.g. lsof, read fdinfo). docs/safety.md Host lock has the table.

From TASK-121 (2026-10-24): express deletion-path hardening as vectors in crates/diskgc-model/tests/vectors/ (walk, races, inuse areas) and use TASK-121's mutation table (its notes) as the baseline: 21 of 23 guard mutations are killed; M7 (walk per-entry mount id) and M17 (judge other_fs) survive (TASK-129). The manual harness can become cargo-mutants on diskgc-core with the corpus test as the oracle. Gaps: non-UTF-8 names (YAML keys), too_many_entries at the real 2M budget, file bind mounts.

Orchestrator 2026-10-24 (COMPASS round 2): Mark ruled 2026-10-24: mutation runs use smaller shards with the guard unchanged. TASK-60 has 644 lines of uncommitted work in tmp/worktrees/task60-harness on a base ~40 commits behind master: reconcile it with HEAD before restarting TASK-60.

Under phase3.1 (2026-10-25): runs as async QA alongside the 0.1 release work, not as a gate; a confirmed deletion-path hole becomes a release blocker task.

QA13 async run (2026-10-25 00:50-03:30 UTC, pinned b009e59 in detached worktree tmp/worktrees/qa13, since removed; no product code edited; no reclaiming diskgc run on the host).
1. Deep review (three read-only mped-architect reviewers: fs path, cargo-target path, in-use/liveness; I checked the key findings against the code). No sequence was found that deletes an unchecked entry, an entry owned by another user, something through a symlink or across a mount (own namespace), a Cargo lock file of a measured Cargo, a Cargo target under a held lock, or a candidate cleared by a stale, failed or unsettled scan (TASK-144's FRESH_ATTEMPTS budget holds on both paths). One contract hole: TASK-165 (release-blocker). An ancestor renamed after reach is never re-reached, so the step-4 keep-marker, place and checkout rechecks judge stale fds and paths, and the candidate is deleted from under a .diskgc-keep or from outside its root. Confirmed by reading only; no vector was written.
Filed: TASK-165 (release-blocker); TASK-166 (fd limit: measured on the host, see 3); TASK-167 (unknown .cargo-*lock* names deep in a profile dir are deleted; safety.md claims otherwise); TASK-168 (judged_last_build is None in production); TASK-169 (blind spots: io_uring/loop/overlay references, mounts in other namespaces, hidepid, non-initial pid ns, btrfs subvolumes, NFS); TASK-170 (guards with no killing test); TASK-171 (17 safety.md drift items plus one PRD wording item for Mark; AC #2 material); TASK-172 (Cargo per-fingerprint part cost); TASK-177 (mutation runner scope). TASK-129 note: M7 is not redundant for file mounts; M17 matters on btrfs.
2. Mutation (cargo-mutants 27.1.0, container with the unchanged guards, see TASK-177): engine/delete.rs 25 = 21 caught, 4 unviable, 0 missed; hostlock.rs 14 = 4 caught, 10 unviable, 0 missed. engine/walk.rs 0/4: the baseline passed, then the 4 GiB RAM floor stopped the run before any outcome. MemAvailable stayed below the 12 GiB start guard for 80 of the ~150 minutes. Not run: the other ~1190 deletion-path mutants and the diskgc-linux mountinfo//proc code.
3. Limits. Entry budget: DEFAULT_MAX_ENTRIES 2,000,000, enforced in each listing (read_dir stops at the remaining budget) and per Cargo target; covered by model tests plus walk/too-many-entries.yaml. FD_WINDOW 16 (at most FD_WINDOW+3 engine fds), model test at depth 60. e2e: engine_deletes_a_100k_entry_30_deep_tree_under_rlimit_nofile_32 and engine_deletes_a_tree_deeper_than_path_max (not run here). Walk EMFILE is an error of the run (model test). Cargo DISCOVERY_DEPTH 6 is pinned by a model test. Gap (TASK-166): the in-use scan holds one pidfd per process, and the binary never raises its soft NOFILE. Measured with a read-only dry run (tmp/target/debug/diskgc, host with 1911 processes): `ulimit -n 1024` (also 512 and 40) made every pytest-tmp candidate skip as in_use with 'pidfd_open <pid>: EMFILE', which is quiet and the opposite of the walk's loud-EMFILE rule. ulimit 2048 was inconclusive in its 40 s window.
AC status: #1 findings are filed (not fixed); #2 is not met (TASK-171, TASK-165); #3 is not met (2 of ~15 files swept); #4 was not assessed. Leave TASK-13 open.

COMPASS round 3 (2026-10-25): dropped the TASK-60 edge (runs async now). Mutation work consolidated into TASK-177.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
