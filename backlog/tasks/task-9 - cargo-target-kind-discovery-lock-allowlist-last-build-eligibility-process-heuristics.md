---
id: TASK-9
title: >-
  cargo-target kind: discovery, lock allowlist, last build, eligibility, process
  heuristics
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 23:20'
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
- [ ] #1 A profile dir with an unknown .cargo-*lock* file is skipped as unsupported_layout
- [ ] #2 Discovery finds targets by CACHEDIR.TAG plus a known lock file, refuses network filesystems, never follows symlinks or crosses mounts (model and container tests)
- [ ] #3 A running test binary, a cargo process in the project dir, and CARGO_TARGET_DIR in a process's initial environment each make the target in_use (container tests)
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
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
