---
id: TASK-9
title: >-
  cargo-target kind: discovery, lock allowlist, last build, eligibility, process
  heuristics
status: Done
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-18 02:25'
labels:
  - wave-1
  - irreversible
  - safety
dependencies:
  - TASK-5
  - TASK-6
  - TASK-8
  - TASK-14
  - TASK-51
references:
  - docs/safety.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: Cargo targets are the biggest cache on the target machines. Implement discovery (CACHEDIR.TAG plus a profile dir with a known lock file, depth limit, no symlinks or mounts, network filesystems refused), the lock allowlist and unsupported_layout, the last-build measure, empty targets, and the process heuristics (exe or cwd in the target, cargo* in the project dir or CARGO_TARGET_DIR in the initial environment). The reclaim protocol is its own task. See docs/safety.md#cargo-target.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A profile dir with an unknown .cargo-*lock* file is skipped as unsupported_layout
- [x] #2 Discovery finds targets by a valid CACHEDIR.TAG plus a profile directory holding any .cargo-*lock* name (an unknown one is then unsupported_layout), refuses network filesystems, never follows symlinks or crosses mounts (model and container tests)
- [x] #3 A running test binary, a cargo process in the project dir, and CARGO_TARGET_DIR in a process's initial environment each make the target in_use (container tests)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-3: Discovery must reach roots with diskgc_core::resolve::resolve (deepest mount, statx-confirmed) and descend with open_beneath (NO_SYMLINKS|NO_XDEV + mount-id check). The mount's fs_type for the network-filesystem refusal is on Resolved.mount (MountEntry from mountinfo). Stat fields are Option (mask-checked); a None mtime on a last-build check is unanswerable, so fail closed.

Forward-carried from task-3 round 5: proc_environ returns Failed, not an empty Environ, for a live process whose environment is empty (env -i); the empty read cannot be told apart from one bound to a dead old leader during a non-leader exec. So the CARGO_TARGET_DIR initial-environment heuristic gets Failed for such processes; treat it per task-6's rule for unreadable processes (conservative: in_use).

Dogfooding run 2026-10-17 (read-only probe of LinuxHost plus ncdu of $HOME, /tmp, /var/tmp; full report in the session scratchpad dogfood/): the probe's discovery skipped hidden dirs and so missed 6.1G of Cargo targets inside ~/.claude/worktrees/*/target (agent worktrees). Discovery must not skip hidden directories wholesale; consider an agent-worktree-target profile.

From task-51: the race tests drive single operations through diskgc-e2e-driver (diskgc-e2e/src/driver.rs); add this task's operation (Cargo discovery / reclaim protocol) as a subcommand in driver::run, generic over H: Delete so its unit tests run on ModelHost, with indexed result lines. Every port call it makes is countable by the controlled wrapper (pause/kill points).

From task-5 (deletion engine): the structural recogniser lives in the walk (diskgc-core engine/walk.rs): an entry matching .cargo-*lock* (is_cargo_lock_name; any lock name, known or not) in a directory P whose parent or grandparent INSIDE the candidate (the candidate itself included) holds an entry named CACHEDIR.TAG makes the candidate contains_special; the tag's signature is not read. Discovery (valid CACHEDIR.TAG, known lock allowlist) is this task's and stricter. Note the kind's own deletion (task-53) must not run the plain walk on the whole target (it would be contains_special by design): walk/delete the entries inside each locked profile directory as separate candidates.

From task-6 (in-use check, diskgc-core inuse): the cargo process heuristics (exe or cwd in the target, a cargo* process with cwd in the project dir, CARGO_TARGET_DIR) should read the same scan, not a second one. inuse::Scan today exposes uses by (dev, ino) (fd, cwd, root, exe, maps), initial-environment paths (values split at ':', absolute components) and per-pid commands (last name of exe); 'exe or cwd inside the target' is already covered by matching the walked target's objects. Missing for this task: per process, its cwd (dev, ino and link text) and exe name in a form the cargo rules can query (cwd inside the project dir = the project dir's walked objects, or path text as a heuristic). Add an accessor on Scan rather than storing derived state. Unreadable processes follow the narrowed rule in docs/safety.md (In use); a Failed environment alone is no environ evidence (task-6 decision), so CARGO_TARGET_DIR of an env -i cargo is invisible.

From task-4: measured in containers (docs/safety.md, Lock files): Cargo 1.99.1 and nightly-2026-10-16 create .cargo-build-lock, .cargo-artifact-lock and .cargo-lock with a plain build and hold .cargo-lock SHARED during a build (the other two exclusive); 1.90.0 holds only .cargo-lock, exclusive. All three names are in CARGO_LOCK_NAMES. The 'running test binary' and 'cargo in the project dir' container tests can use the race fixture and the runner gate of diskgc-e2e/src/cargo_under_test.rs; note that during a cargo run pause the paused process is the runner wrapper (cargo's pid, exe diskgc-e2e), not a process named cargo (see task-11 note).

From the task-4 review (mped, reproduced by task-4; docs/safety.md, Lock files and Known gaps): (1) Last build reads .fingerprint/ and deps/, which nightly-2026-10-16's default new build-dir layout does not have (build/<package>/<hash>/{fingerprint,out} instead); revise the measure with task-53. (2) With CARGO_BUILD_BUILD_DIR (1.99.1, nightly; 1.90.0 ignores it) the build-dir is a second CACHEDIR.TAG directory holding only .cargo-build-lock, and cargo test runs test binaries from it. The rule 'a cargo* process with cwd in the target's project directory (its parent)' does not cover a build-dir outside the project; the running test binary itself is still caught by exe-inside-target. A rule on CARGO_MANIFEST_DIR in the initial environment (task-11 note (7)) would partly cover it.

[Superseded in part by the task-8 review round below: IdleFor::admits, the hand-written visitors and a source's own idle_for are gone.]
From task-8 (profile model): the built-in cargo-target profile is {kind: cargo-target, find: {roots: required}} (diskgc-core profile/builtin.yaml); its Source.finds is Finds::CargoTargets and its action Action::CargoLockProtocol. Roots come from Library::bind_roots(&checked, tier roots or --roots) -> map name -> roots. The idle measure is profile::IdleMeasure::LastBuild ('last build', the name for reports); compute the last-build Timestamp here (None when any needed mtime is unanswered: fail closed) and pass it to IdleFor::admits(last_use, now), with the term's requirement from Library::idle_for(&term) (longest of the source's own and its filter:s). The Host port has no realtime clock yet; 'now' must come from somewhere (a port op). verified_with currently names the Cargo versions whose lock sets were measured; update it when discovery and the race tests (task-11) are verified.

From task-8's review: IdleFor::admits is replaced by IdleFor::verdict(last_use, now) -> Idleness (Idle / NotIdle / Unknown); pass None when a last-build mtime is unanswered (Unknown::Unanswered, skip unsupported_fs), and a future last build is Unknown::InFuture (skip future_time). Overlapping roots (one target reachable from two given roots) are not deduped by the profile layer; dedupe by identity in discovery (task-61 (39)).

Implementation (2026-10-18, implementer; not committed, awaiting the deep review gate). Code: diskgc-core cargo_target (discover.rs, eligible.rs, evidence.rs, mod.rs). Rules: docs/safety.md, cargo-target (Lock files, Discovery/idleness/eligibility, In use).

Design decisions (COMPASS questions a-e settled here):
(a) Layouts. Last build is now layout-independent: the newest mtime OR ctime of the profile directories, their lock files and everything the entry walks recorded (reference.md idle_for table updated). Layout (cargo_target::Layout) only says where fingerprints are, for task-53: Classic = .fingerprint/ or deps/ in the profile; PerUnit = neither, build/ present (nightly default); a non-empty profile in neither is unsupported_layout (fail closed). Unknown age is never idle: unanswered time -> unsupported_fs, future -> future_time (IdleFor::verdict).
(b) Split build dir. Lock sets are allowlisted too: whole = {.cargo-lock}, {+.cargo-build-lock}, {+both}. {.cargo-build-lock} = build-dir half, {.cargo-lock,.cargo-artifact-lock} = target-dir half, anything else unmeasured: all unsupported_layout. Decision: NEITHER half of a split pair is reclaimed: nothing on disk links the halves, and locking one alone leaves cargo test's compile-to-launch window (cwd = project, not the build dir's parent) unprotected. Limit: projects with a separate build dir are never reclaimed until pairs can be recognised.
(c) Process heuristics (cargo_target::cargo_evidence, on task-6's scan; all evidence listed, in_use detail '; '-joined): 1 uses (fd/cwd/root/exe/map) of the target dir, dirs directly in it, profile dirs, lock files by identity; 2 cwd/exe link text inside the target; 3 cargo* (Nix wrapper normalised) with cwd at/below the project dir (text) or the project dir itself (identity); 4 any initial-env path inside the target (CARGO_TARGET_DIR, CARGO_BUILD_BUILD_DIR, LD_LIBRARY_PATH, OUT_DIR); 5 CARGO_MANIFEST_DIR at/below the project dir (mped's rule; cargo run's exec'd runner). Plus every entry record checked with Scan::check, and Scan::blocked (unsettled / blocking unreadable).
(d) Discovery walks hidden dirs; depth 6 (root = 0); valid CACHEDIR.TAG (signature read through the O_PATH fd: new port op Host::read_head); profile dir = any .cargo-*lock* name at <t>/<p> or <t>/<x>/<p> (so an unknown-only lock set is reported unsupported_layout, not missed); never descends into a found target; never follows symlinks or crosses mounts; network-fs root refused unread (engine::NETWORK_FS_TYPES).
(e) Dedupe by (dev, ino) across roots, first root wins, others in Discovery.duplicates.
Also: keep markers count from the root down to the target's parent, in the target and dirs directly in it, and in the walked entries; ownership of target dir, triple dirs, profile dirs and lock files is required; eligibility takes no lock (that is task-53 step 1). New port op Host::wall_clock (CLOCK_REALTIME) for the idle comparison.

Measured (containers, 2026-10-18, all three toolchains; docs/safety.md Lock files): cargo check makes {.cargo-build-lock,.cargo-lock} on 1.99.1 and the nightly; runner/test env carries CARGO_MANIFEST_DIR, CARGO, LD_LIBRARY_PATH naming the profile dir (1.90/1.99 also deps/; split 1.99 names the build dir's deps/, split nightly only target/debug) and on 1.99.1 OUT_DIR; nightly test binaries run from build/<pkg>/<hash>/out/.

Tests: model crates/diskgc-model/tests/cargo_target.rs (23 tests, each rule bitten by a mutate-run-restore: 27 mutations, all failing their test); driver diskgc-e2e-driver cargo-targets [--idle-for DUR] ROOT... (model unit test driver::tests::cargo_targets_reports_discovery_and_eligibility); container tests per toolchain in diskgc-e2e/src/cargo_target_suite.rs: cargo_targets_discovered_and_judged, cargo_split_build_dir_halves_are_unsupported_layout (vacuous on 1.90.0, logged and checked), cargo_running_cargo_keeps_its_target_in_use. Container bites shown: evidence disabled, lock-set rule disabled, hidden dirs skipped -> each test FAILs. Race fixture's #[test] can now wait at DISKGC_E2E_TEST_GATE (a test binary still running).

AC #1: an_unknown_lock_name_is_unsupported_layout (model), cargo_targets_discovered_and_judged@* (container). AC #2: discovery_finds_targets_also_in_hidden_directories_and_nothing_else, overlapping_roots_find_each_target_once, a_root_on_a_network_filesystem_is_refused_unread (model; NFS is model-only, docs/testing.md), cargo_targets_discovered_and_judged@* (container: hidden dir, dedupe, a symlink and a same-fs bind mount not followed). AC #3: a_running_test_binary_is_in_use, a_cargo_process_in_the_project_directory_is_in_use, cargo_target_dir_in_the_initial_environment_is_in_use (+ the other rule tests) in the model; cargo_running_cargo_keeps_its_target_in_use@* (container: blocked cargo build, cargo run and cargo test paused in their runner, a running test binary, CARGO_TARGET_DIR).

Limits: discovery cost is a statx per entry within depth 6 of every root (no pacing yet: the tally's job); inspection lists every directory one level inside a target (target/doc crates, target/tmp); a mount one or two levels inside a target makes it contains_mount; a test fixture with a .cargo-*lock* in target/tmp/<x>/ would count as a profile dir; heuristics are path-text based where noted (other mount namespaces only by identity); builtin.yaml verified_with not changed (task-11 when the race tests verify).

Round 2 (deep gate round 1: Codex NO-GO x2, mped NO-GO x1; all addressed, still uncommitted):
- B1 (Codex 1): layout evidence must be real directories. ProfileDir.entries are now ProfileEntry {name, file_type} (statx, not followed); Layout needs a directory .fingerprint/deps (classic) or build (per-unit); a regular file or symlink of those names is unsupported_layout; an entry whose type changed between inspection and its walk is changed (that re-check is defence in depth, not exercised by a test). Test: layout_markers_must_be_directories (file, symlink, directory control for each name).
- B2 (mped B1): discover walks roots outermost first (stable sort by component count), so a target is recorded under the outermost root and reach() counts markers on its whole way down. Test: a_keep_marker_above_an_inner_root_counts_in_either_root_order; overlapping_roots_find_each_target_once now expects the outermost root. Remaining limit (safety.md): a target reached through two unrelated paths (bind mount) counts only the recording root's markers.
- B3 (Codex 2 / mped S4): evidence::undeleted, is_cargo and inuse::scan::command_of work on bytes (no from_utf8_lossy left in a decision; the remaining lossy conversions in cargo_target/ and inuse/ only build messages). Tests: unit non_utf8_paths_and_names_are_compared_as_bytes; container cargo_running_executable_below_a_non_utf8_name_keeps_its_target_in_use@* (target under /tmp/w/\xff, busybox copy running from target/tmp with cwd / and env PATH only: only the exe-text rule fires).
- S1: CORRECTION of my round-1 limit note: the idle path IS testable in containers (a ctime is the build's time). cargo_targets_discovered_and_judged now waits until 1.2 s after the newest change, expects eligible with --idle-for 1s, and checks every eligible line's last_build= against an independent std walk (max mtime/ctime of target/debug and everything below) on every toolchain (per-unit on the nightly).
- S2 (mped): detached targets. reach() records Project {id, detached}: detached when the target's parent holds no Cargo.toml (or that cannot be told). Evidence rule 6: any visible cargo* process blocks a detached target, wherever it works (covers --target-dir, CARGO_TARGET_DIR without env evidence, target-dir in .cargo/config.toml, nested targets). Remaining gaps written in safety.md cargo-target / In use (--manifest-path with an attached target, invisible Cargos, other mount/PID namespaces). Tests: a_cargo_process_anywhere_keeps_a_detached_target (model; attached and non-cargo controls), cargo_detached_target_is_in_use_while_any_cargo_runs@* (container).
- S3: LinuxHost::read_head fstats the O_PATH fd first and refuses a non-regular file before anything is opened, then reopens through /proc/self/fd. Test read_head_refuses_a_fifo_without_opening_it: an inotify IN_OPEN watch sees no open of the FIFO (O_PATH opens make none) and one for the regular-file control. A device node cannot be made unprivileged on the host; it takes the same type check.
- AC #2 reworded to match the code (any .cargo-*lock* name).
Bites round 2: model/unit mutations B1, B2, B3, S1 (last build +1 h), S2, S3 each fail their test; container mutations B3, S1, S2 each fail their container test with their own message.

Round 3 (Codex round-2 blocker, mped GO): a .cargo-*lock* name counts as a lock file only on a regular file (statx, not followed); a directory, symlink, FIFO, socket or char device with such a name makes the target unsupported_layout (judge_layout, checked first), so nothing in that profile counts as reclaimable; a lock replaced by another type later is changed (same_locks compares identity, type included). Tests: a_lock_name_on_anything_but_a_regular_file_is_unsupported_layout (model, all five types plus the regular-file control), cargo_targets_discovered_and_judged@* (container: a real build's .cargo-lock replaced by a FIFO and by a directory). The round-2 type-change guard now has its own test: a_layout_marker_whose_type_changes_before_its_walk_is_changed (model events swap build/ for a file right before the walk opens the profile directory). Docs: cargo_target module doc (entries keep names and types), safety.md lock files are regular files, and the blind spot of another project's target-dir pointing at an attached target. Bites: lock-type rule disabled -> model test fails (Codex's scenario judged Eligible, valuable-data reclaimable) and container test fails (/tmp/w/lf/target eligible); type-change guard disabled -> its model test fails.

Round 4 (Codex round-3 blocker, mped GO): a target is attached only when its parent holds a Cargo.toml that statx (not followed) shows as a regular file; a directory or a symlink (dangling or not) of that name leaves it detached (the conservative side; this drops the earlier acceptance of symlinked manifests). Test only_a_regular_cargo_toml_makes_a_target_attached (directory, symlink to a regular file, dangling symlink: in_use by a cargo elsewhere; regular-file control eligible); bite: has_manifest accepting any type -> the directory case is Eligible, test fails. Regression a_lock_file_replaced_by_a_symlink_after_discovery_is_changed (a rename over .cargo-lock between discovery and the check -> changed); bite: same_locks by name only -> the test fails (the target is then still refused, as unsupported_layout by the lock-type rule). safety.md says what attached means; doc nits fixed (detached-target rule named, not numbered; long doc lines rewrapped). QA addendum: the gate message and docs in diskgc-core host.rs and diskgc-linux sandbox.rs now say development builds keep the gate and release packages from 0.1 remove it (no 'until v1' left).

Committed as ec38916 (tree ff37313) after the deep gate's fourth round: qa-test-runner, mped-architect and Codex all GO. Decision to raise at the check-in: CARGO_BUILD_BUILD_DIR split halves are never reclaimed in 0.1. In the task-53 handoff note above, Reached.project is Project {id, detached} (the later line supersedes the earlier one).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
