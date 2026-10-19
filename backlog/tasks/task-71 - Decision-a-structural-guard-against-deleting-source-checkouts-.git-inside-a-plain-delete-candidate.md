---
id: TASK-71
title: >-
  Decision: a structural guard against deleting source checkouts (.git inside a
  plain-delete candidate)
status: Done
assignee: []
created_date: '2026-10-18 11:29'
updated_date: '2026-10-18 23:19'
labels:
  - wave-2
  - spike
  - safety
dependencies: []
references:
  - docs/safety.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: task-68's dry run of cmake-build as reference.md described it ("directories containing CMakeCache.txt") found, as the largest eligible candidate, ~/topics/cmake_nix_backend/CMake: an in-source CMake build, i.e. a git checkout (.git, CMakeLists.txt, Source/) with uncommitted changes, 5.6 GiB. A tier selecting cmake-build would have deleted the user's work. task-68 fixed the row (has_not: CMakeLists.txt), but the class remains: any marker-file profile (has:) or broad pattern under a project root (cachedir-tagged, a pytest-cache with a wrong depth, a user's `*`) can match a source tree, and the eligibility walk has no notion of "this is someone's work". The PRD accepts that a wrong profile deletes "unused, own files"; whether a VCS checkout should be one is the question.

Decision for Mark: (a) leave it to profiles and examples (status quo, with the has_not fix); (b) a fixed structural recogniser like the Cargo one: a .git (directory, or file as in worktrees and submodules), .hg or .svn anywhere in a plain-delete candidate makes it contains_special (cost: a name check in the walk; misses: caches that hold checkouts, e.g. ~/.cargo/git, Go's module cache, would be kept too); (c) only at the candidate's top level (cheaper, narrower). For (b) or (c): docs/safety.md (The eligibility walk) and the engine's recogniser, with model tests.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The decision (Mark's) is recorded in docs/safety.md or the PRD
- [ ] #2 If a guard is chosen, a model test shows an in-source build (.git and CMakeCache.txt) is never eligible for a plain delete
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
QA's real-binary evidence (task-68 review round 1): proj/insrc2, a directory holding .git and CMakeCache.txt but no CMakeLists.txt, is eligible for cmake-build even with the fixed row (has: CMakeCache.txt, has_not: CMakeLists.txt); and tmp-entry plans as eligible a /tmp directory holding two .git trees. The has_not fix narrows the observed case only; the class needs the guard decided here.

Decision by Mark (2026-10-18): yes: refuse any plain-delete candidate that has .git inside (at any depth the walk sees). It only adds safety; the task-68 spike found a real near-miss (cmake-build picked an in-source checkout with uncommitted work). Implement as a skip reason in the fs eligibility walk, in the dry-run planner and in task-18's delete path; document its single home in docs/safety.md.

Scope extended by Mark (2026-10-18, relayed by the orchestrator): refuse .hg and .svn exactly like .git (directory, regular file or symlink, matched by name, never followed); same skip reason contains_checkout. Reasoning: the same asymmetric safety logic, they mark uncommitted VCS work, refusing them only adds protection.

Implemented (task-71 implementer): engine::walk refuses any entry named .git, .hg or .svn (engine::CHECKOUT_MARKERS), of any type, anywhere in the candidate, the candidate itself included, as the new skip reason contains_checkout (one name compare per entry, no extra syscall; the walk stops at the marker and never reads inside it). Fail closed by construction: engine::walk always has the guard; the only opt-out is the crate-private engine::walk_cargo_profile_entry, called only by the Cargo kind's per-entry walk (cargo_target::eligible::walk_entries, shared by eligibility and the reclaim protocol). The fs dry-run planner (task-68) and task-18's future fs delete path go through engine::walk. Rule's single home: docs/safety.md, The eligibility walk (No source checkout), with the Cargo exemption, the accepted cost and the limits; reference.md lists the reason and the cmake-build row points at it. Correction to the brief: Go's module cache vcs dirs (pkg/mod/cache/vcs/<hash>) are bare repositories with no .git entry (checked on this machine), so they are NOT kept by the guard; bare repositories in general are not recognised (documented as a limit).

Read-only dry runs on this laptop with the guard (2026-10-18; ensure --dry-run, spike-profiles.yaml, nice/ionice, --timeout 10m):
- cmake-build (fixed row, roots $HOME and $HOME/topics): 13 eligible, 2 contains_checkout (task-68: 15 eligible). Both are real CMake build dirs that fetch dependencies: tracy/build (_deps/pybind11-src/.git, FetchContent, 12M) and tracy/capture/build (.cpm-cache/.../.git, CPM, 448M). Cost of the decision: FetchContent/CPM build dirs are never reclaimed by cmake-build. Documented in safety.md (Kept too).
- cmake-build with the OLD row (has: CMakeCache.txt only, a scratch config): 19 eligible, 3 contains_checkout, including task-68's near-miss cmake_nix_backend/CMake (5.5G by du), refused at its .git. The guard alone catches the original incident.
- tmp-entry: deadline after 26,122 candidates judged (of ~155k): 26,062 eligible (0.63 GiB), 41 contains_checkout, 8 contains_special, 7 not_owner, 2 in_use, 2 contains_ipc. The 41 are mostly agents' scratch clones (/tmp/fix-*, *-review, *-qa); 40 of them hold about 0.9 GiB (du); the 41st is /tmp/claude-1000, now contains_checkout (a scratchpad clone found before the root-owned log that made it not_owner in task-68).
Question for Mark (not blocking): agent scratch clones in /tmp and FetchContent/CPM build dirs are now never reclaimed by a plain delete. Intended? If not, a narrower exemption would need its own decision (e.g. a checkout whose work is all pushed), not a profile.

Round 2 (Mark's decisions, 2026-10-18):
- D1 command actions: the checkout guard stays fail-closed for them too, until a real case needs an exception (over-protecting trusted code is the safe default). safety.md now scopes the rule to every candidate except cargo-target's, command actions included. Noted on task-37.
- D2 pytest-tmp granularity: one candidate per pytest session (pytest-of-<user>/pytest-<N>, also one level down in nix-shell.* and tmp.* dirs), not the whole pytest-of-<user>; the pytest-current symlink is never a candidate (type: dir), the pytest-of-<user> dir itself is left alone. reference.md (example, library row, note), spike-profiles.yaml, architecture.md granularity. Noted on task-65. (pytest names its symlink pytest-current, inside pytest-of-<user>; seen on this machine. The brief's pytest-<user>-current does not occur here.)
- D3 PRD: checkouts added to the Safety boundaries bullet and the Nesting decision row.
- Path rule approved (Mark): a candidate whose path has a .git/.hg/.svn name between its profile root and itself (proj/.git/objects under a broad pattern) is contains_checkout, a path-only check in the fs planner carried down like the keep markers; cargo-target exempt. Implemented here instead of task-72 (closed, pointing here).
QA F6 ("2 candidate(s)" then a third): not a counting bug. The count is what was judged before the deadline stopped the plan; the third path is the run error naming the entry the deadline hit, which is never planned. Reproduced on ModelHost (tmp-entry over four dirs, deadline at each of four points: count == plan entries each time). The log line now says "judged" and, when stopped, "discovery stopped early".
Dry run pytest-tmp on this laptop (read-only, same binary, guard on in both): whole pytest-of-* (old profile): 3,133 eligible (3.37 GiB), 1,436 contains_checkout, 38 contains_ipc; per session (new): 3,034 eligible (3.41 GiB), 1,542 contains_checkout, 38 contains_ipc, 1 in_use. Most pytest-of-* here are inside nix-shell.* temp dirs holding a single session, and most sessions that hold a checkout hold it themselves (tests running git init), so per-session splitting frees little extra here; it matters where one pytest-of-<user> collects many sessions (/tmp/pytest-of-<user> keeps three).

Round 3 (Mark, 2026-10-18): pytest-tmp is one candidate per TEST DIR (pytest-of-*/pytest-*/*, plus nix-shell.*/ and tmp.*/ one level down; type dir). The guard stays intact; only test dirs that ran git init are kept. Reason (mped's read-only measurement): per session, 1,559 of 4,670 sessions held a .git, about 21 GiB, about 6x the 3.41 GiB pytest-tmp could reclaim. Session dirs, pytest-of-<user>, the *current symlinks and plain files in a session dir are never candidates (reference.md). Also: the path rule is now a pure function, engine::checkout_metadata_below(root, at) (mped N1), with a unit test; the Above struct is gone.

Dry run 2026-10-18, this laptop, read-only (nice/ionice, --timeout 20m), per test dir: DEADLINE after 1,224 s with 43,766 test dirs judged in 595 of ~4,670 sessions, so the totals are partial and not comparable with the complete per-session run (3,034 eligible, 3.41 GiB, 1,542 contains_checkout). In the judged part: 32,441 test dirs eligible, 784,044,032 bytes (0.73 GiB, diskgc's estimate); 11,293 contains_checkout, 3.13 GiB by du; 31 contains_ipc, 1 in_use. What the split itself unlocks there: 11,669 eligible test dirs, 0.32 GiB, in 149 sessions that also hold a checkout (kept whole per session before); 383 sessions had no checkout (0.41 GiB), 63 had only checkout test dirs. Findings: (1) most of the bytes the guard keeps are in the git-init test dirs themselves, not in their siblings, so the per-test-dir split recovers the siblings only (here about a tenth of what the guard keeps); (2) the candidate count rises about tenfold (about 74 test dirs per session here), and a dry run of all of /tmp's pytest dirs no longer fits 20 min: buckets (architecture.md) and task-70's scan reuse matter for this profile. Not extrapolated to the whole of /tmp.

Corrections (task-80, from mped's round-3 review): (1) Finding (2) above overstates buckets: a name family keeps the test name (test_foo0, test_bar0, ...), so per-test-dir candidates of pytest-tmp almost never merge into one bucket, and buckets do not absorb the tenfold candidate count; only task-70 (scan reuse) addresses that cost. (2) The 0.73 GiB and 3.13 GiB figures are of different sets (the eligible test dirs, and the contains_checkout test dirs) and from different tools: diskgc's estimate (stx_blocks x 512 per name, directories included) and du (allocated blocks per inode, directories included). Both count allocated blocks; they differ only where a file has several hard links in the set (diskgc counts it once per name, du once), which pytest test dirs rarely hold. Not checked against the data: no run measured the same set both ways. (3) The Cargo exemption described above (engine::walk_cargo_profile_entry) is gone: since task-80 the guard is the source profile's checkouts setting (refuse by default), carried in engine::Rules::checkouts, and the built-in cargo-target says checkouts: allow in builtin.yaml.

Follow-up (Mark, 2026-10-19, task-80): pytest-tmp now says checkouts: allow, after a survey of all 75,253 leftover repos in pytest sessions found only test fixtures (task-65 note). The git-init test dirs counted above as contains_checkout are eligible under that profile; the per-test-dir granularity stays for now (task-70).
<!-- SECTION:NOTES:END -->
