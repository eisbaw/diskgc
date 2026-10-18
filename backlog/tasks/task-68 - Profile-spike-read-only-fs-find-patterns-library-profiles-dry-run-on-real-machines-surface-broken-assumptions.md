---
id: TASK-68
title: >-
  Profile spike (read-only): fs find patterns + library profiles dry-run on real
  machines, surface broken assumptions
status: Done
assignee: []
created_date: '2026-10-18 07:44'
updated_date: '2026-10-18 13:00'
labels:
  - wave-1
  - spike
  - profiles
dependencies:
  - TASK-10
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: Mark (2026-10-18) wants the profile library exercised now, before more hardening, so that wrong assumptions in the PRD, profile schema, selection grammar and kind model surface early instead of after the wave-1 hardening and the schema freeze. Read-only: no deletion path is added or changed here. The deep-gated delete work stays in task-18, and the full selection engine with its property tests stays in task-16.
Scope: implement just enough segment-wise find-pattern matching (no **) for the fs kind's discovery and eligibility, reusing the engine walk and the in-use checks, so that ensure --dry-run --select profile:X plans fs candidates. Write the library profiles from docs/reference.md (Built-in profile library) as user profiles in a scratch config: the /tmp group (pytest-tmp, nix-shell-rc, nix-shell-tmpdir, claude-code-tmp, chromium-tmp, mktemp-dir, tmp-entry, playwright-tmp, node-compile-cache, zephyr-tmp, renode-tmp) and the roots-required ones that are plain deletes (pytest-cache, cmake-build, west-build, jest-cache, platformio-cache, gradle-caches, maven-repo). Command-action profiles (pip/go/npm/ccache/uv/pnpm/yarn) are written as YAML and validated but not run. Dry-run them on this machine (read-only, nice/ionice) and compare with ncdu/du for the same paths.
Output: a findings list. For every profile, which schema, grammar or kind assumption it could not express or got wrong (any-depth finds without **, per-uid paths, PrivateTmp, ownership, idle semantics for plain dirs, in-use false positives, etc.). Each finding is filed as a backlog task or recorded as a PRD/docs question, and the docs are fixed where the answer is clear.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 fs find patterns plan candidates in ensure --dry-run for user profiles, with model tests that bite (red without the matcher, green with it)
- [x] #2 Every listed library profile exists as a user profile with examples; those that the schema cannot express are listed with the reason
- [x] #3 A read-only dry run of the profiles on this machine is recorded in the task notes (eligible counts and sizes per profile, skip reasons), with no reclaiming run
- [x] #4 Each broken or questionable assumption is filed as a task, or as a docs/PRD change in this task, with the decision named as Mark's where it needs one
- [x] #5 No change to any deleting code path (Delete port, engine delete, cargo reclaim, sandbox gate)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Spike results (implementer, 2026-10-18).

CODE: crates/diskgc-core/src/fs_kind/ (pattern.rs matcher; plan.rs discovery + judgement, read-only, given a Host only); executor: Tier.source (SourceKind::CargoTarget | Fs(Find)), dry_run branches plan_cargo / plan_fs; ensure refuses kind fs first thing (usage error, exit 2, names task-18, no host call at all). Loader refuses `?[]\` in patterns. Tier::select refuses has/has_not term filters (task-16) instead of ignoring them. The fs dry run reuses a process scan for 60 s (DRY_RUN_SCAN_REUSE; a blocking scan only within REUSE_LIMIT); inuse::Checker (clamped to FRESHNESS) and every deleting path are untouched. Tests: crates/diskgc-model/tests/fs_dry_run.rs (9), fs_kind::tests (8); 16 mutate-and-restore bites all red.

PROFILES (the evidence): crates/diskgc-core/src/fs_kind/spike-profiles.yaml, a config of 27 fs sources incl. 7 command actions (loaded, never run) and probes; fs_kind::tests loads it and checks every example.

DRY RUNS on this laptop, read-only only: `nice -n 19 ionice -c3 diskgc ensure --dry-run --json --timeout 20m --min-free 50G --config spike-profiles.yaml --select 'profile:X' [--roots $HOME --roots $HOME/topics] /`. / is one ext4 (LUKS) with /tmp and /home on it, 98 % full; /tmp had ~155,000 top-level entries; load average 10 to 90 during the runs (agents' builds), so runtimes are upper bounds. Sizes: diskgc's estimate vs du -s over the same eligible paths.
profile | judged | eligible | MiB (diskgc / du) | skips | runtime
node-compile-cache | 328 | 328 | 28 / 28 | - | 7 s (586 s with a scan per candidate)
chromium-tmp | 268 | 257 | 9 / 9 | contains_ipc 11 | 4 s
renode-tmp | 1,891 | 1,891 | 34 / 34 | - | 22 s
playwright-tmp | 71 | 71 | 0.3 / 0 | - | 3 s
mktemp-dir | 659 | 658 | 145 / 145 | not_owner 1 | 8 s
zephyr-tmp | 2,517 | 2,517 | 2,175 / 2,175 | - | 71 s
pytest-tmp | 4,343 | 4,185 | 10,708 / 10,699 | contains_ipc 158 | 429 s
claude-code-tmp (-*/*) | 308 | 295 | 2,794 / 2,599 | in_use 6, not_owner 2, contains_special 2, contains_ipc 3 | 49 s
claude-code-tmp-star (old */*) | 344 | 331 | 2,941 / 2,747 | same | 17 s
nix-shell-tmpdir | 41,347 | 41,145 | 11,228 / 11,219 | contains_ipc 199, in_use 2, unreadable 1 | 1,099 s
nix-shell-rc | 65,458 of ~105k (deadline, exit 124) | 65,458 | 350 / 349 | - | 1,204 s
nix-shell-rc && idle-3d | 66,643 (deadline) | 59,817 | 314 | not_idle 6,826 | 1,201 s
tmp-entry | 79,484 of ~155k (deadline) | 79,465 | 3,768 | not_owner 8, contains_special 8, contains_ipc 2, in_use 1 | 1,204 s
tmp-entry && idle-2d | 41,655 (deadline) | 37,813 | 807 | not_idle 3,820, in_use 4, + as above | 1,205 s
pytest-cache | 63 | 63 | 13 / 13 | - | 7 s
cmake-build (old row) | 22 | 22 | 7,496 incl. a 5.6 GiB in-source git checkout | - | 6 s
cmake-build (fixed row) | 15 | 15 | 1,914 / 1,914 | - | 21 s
west-build (fixed) | 5 | 5 | 641 / 641 (all plain CMake build/, none Zephyr) | - | 13 s
jest-cache, platformio-cache, gradle-caches, maven-repo | 0 | 0 | - | - | 1-2 s each (none on this laptop)
cachedir-tagged (probe) | 188 | 185 | 8,039 / 6,987 (uv .venv hard links) | contains_special 2, in_use 1 | 38 s
No reclaiming diskgc was run. No error other than the deadline. Surprises: no false not_owner; PrivateTmp never touched; symlinks/FIFOs never candidates; mounts never statted; in_use came from Claude processes holding task output files, a Bun .node mapped by Claude, two live nix shells (TEMPDIR=), fontconfig, and (old binary) unsettled scans under load.

FINDINGS, and what was done with each:
1. Glob syntax within a segment was undefined: defined in reference.md (find.paths row): `*` only, matches a leading dot, bytes; `?[]\` reserved (load error). Python tempfile dirs (tmpXXXXXXXX, 3,457 here) need `?` or classes: task-16 note.
2. Any-depth without `**`: explicit depths work and are cheap here (6-38 s); evidence on task-16 (AC #4 decides).
3. Nested matches of one profile: the spike never descends into a candidate (outer wins): decision on task-16.
4. has / has_not meaning: reference.md now says a regular file directly in the candidate dir. Failed-has_not reason and term-filter has: task-16. `where` cannot say "CMakeCache.txt plus zephyr/": task-16 / task-41.
5. cmake-build as documented selects in-source builds (a git checkout with uncommitted work, 5.6 GiB): row fixed (no CMakeLists.txt), west-build too; general guard: task-71 (decision for Mark).
6. Cruft lives inside TMPDIR containers (nix-shell.*, rc dirs, zephyr run dirs, /tmp/claude-{uid}, nested shells); every profile repeats container patterns: task-69 (decision for Mark).
7. claude-code-tmp `*/*` also matched inside Claude's TMPDIR junk: row fixed to `-*/*`. chromium-tmp and playwright-tmp rows did not match this machine: fixed. Details: task-39.
8. In-use scan cost and fork-heavy load: a scan per candidate (Checker's <= 1 s reuse) made dry runs 80x slower; unsettled scans give false in_use; a reclaim of 40k-105k small candidates does not fit a 20 min timeout: task-70 (and task-18 note). Dry-run reuse documented in reference.md (--dry-run).
9. Coarse candidates are pinned by one bad entry: /tmp/claude-1000 (2.7+ GiB) is not_owner for tmp-entry because of one root-owned log in one session; nix-shell-tmpdir and pytest-tmp lose 199 / 158 dirs to stale FIFOs and sockets under the default contains_ipc policy: task-39 note; the "live only" policy is not reachable until the schema defines it (docs/safety.md).
10. fs estimates count hard links per name (claude-code-tmp +7 %, cachedir-tagged +15 %: uv venvs are hard links into uv's cache, so deleting them frees little): task-18 note.
11. Examples of roots: required and {uid} profiles, and of where-narrowed profiles, are weak or impossible to check by path: task-17 note.
12. jest's cache is $TMPDIR/jest_*, not roots: required; gradle/maven/platformio/jest absent here, so unverified: task-40 note. CACHEDIR.TAG marks .venv, .pytest_cache, .ruff_cache, .mypy_cache, cargo registries: usefulness as a filter is a decision on task-40.
13. Live Claude session scratch is protected only incidentally (open output files of background tasks; our own session dir was skipped only for a Cargo lock name in an old scratch target): consistent with the PRD risk (protect claude-code-tmp); recorded on task-39.
14. Dry-run JSON for tmp-entry: 79k plan entries, 14.6 MB on stdout; buckets (reference.md Output) will be needed for /tmp: task-39 note via this list.

Review round 1 fixes (round 2): F1 ensure's guard is an allowlist (only SourceKind::CargoTarget reclaims) and executor::discover matches Tier.source exhaustively (no _ arm), so an fs tier never reaches the Cargo reclaim: model test an_fs_tier_never_reaches_the_cargo_reclaim_over_its_roots (QA's scenario). Bites: guard removed alone, only the refusal test is red and nothing is unlinked; guard and discover check removed, the defence test is red too. F2 mounts inside a candidate are refused before its own where looks inside (test a_mount_inside_is_refused_before_the_sources_where_looks_inside). F3 a root inside systemd-private-* is never opened (a_root_inside_private_tmp_is_never_searched). F4 deadline checked after every listing and before every statx (discovery_stops_at_the_deadline_after_a_listing_and_between_statx_calls, exit 124). Notes filed on task-16, 20, 26, 28, 65, 70, 71. 21 of 22 bites red; the green one is the discover check with the guard in place, shadowed by design.
<!-- SECTION:NOTES:END -->
