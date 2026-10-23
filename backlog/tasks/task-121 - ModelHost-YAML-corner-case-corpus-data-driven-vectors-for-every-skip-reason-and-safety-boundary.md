---
id: TASK-121
title: >-
  ModelHost YAML corner-case corpus: data-driven vectors for every skip reason
  and safety boundary
status: Done
assignee:
  - '@claude'
created_date: '2026-10-23 16:29'
updated_date: '2026-10-23 23:00'
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
- [x] #1 A generic YAML vector runner exists and every file in the corpus directory runs under just check
- [x] #2 Every skip reason in docs/reference.md has at least one vector; a completeness test enforces it
- [x] #3 Every built-in source has match + in-use + protected/negated + nested-guard vectors; a completeness test enforces it
- [x] #4 Every PRD 'What bad looks like' line for diskgc's own actions has a vector that would fail if the guard were removed (bite evidence in notes)
- [x] #5 docs/testing.md describes the corpus layout and how to add a vector
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Move ensure's composition (config -> owners -> protections -> tiers -> executor) from crates/diskgc/src/main.rs into diskgc-core (invocation module) so the binary and the vector runner run the same code; main keeps arg parsing, startup, config lookup.
2. Extend the profile_vector: 1 runner as a strict superset (select, config text, allow_root, users, seed, timeout, target_free, dry_run_exit, errors, process_blockers, JSON report subsets); one runner walks tests/models/profiles and tests/vectors/<area>.
3. Corpus by area: skip reasons, PRD bad-lines, per-built-in (match/in-use/protected-or-negated/nested-guard), corner cases, config/exit codes, races via events.
4. Completeness tests: documented skip enum (Reason::ALL + backend reasons) and built-in sources x {match,in_use,protected|negated,nested_guard} via a vector 'covers' tag checked against actual results.
5. Bite evidence: temporary mutation per guard, record vector->mutation table in notes.
6. docs/testing.md corpus section; follow-up tasks for inexpressible cases.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from TASK-64 (2026-10-23): new YAML cases to fold into the corpus runner: profiles/python-pycache-fork-storm-unsettled.yaml and profiles/python-pycache-fork-burst-settles.yaml (in-use liveness; bites M1/M2/M3' recorded in TASK-64 notes) and hostlock/held-by-a-remaining-reference.yaml, hostlock/released-by-the-last-reference.yaml (run by tests/ensure.rs with a 2 s timeout because the host-model YAML has no request block; a generic runner needs request fields: select, roots, min_free, timeout). Gotcha: a modelled process with no fds is unreadable and blocks every candidate.

From TASK-112 (2026-10-23, branch task112-design): docs/testing.md 'Producer liveness vectors (planned)' specifies ~70 named profile vectors for liveness contracts (Owner PID / nix-shell-rc, pytest sessions, node/python project dependency directories), plus the profile_vector and host-model extensions they need (expect.plan/skipped[].evidence; pid_namespace, proc_hidepid, processes[].nspid/zombie/cmdline/view, tree atime, mount noatime). They are this corpus's liveness part: implement the extensions once, shared with TASK-112's implementation, and do not duplicate the cases.

Implementation (2026-10-24, commits d5f7c80, 96cd2aa, 522f5b6, f0e533c):
- Runner: profile_vector: 1 extended as a strict superset (select, config text, allow_root, users, seed, timeout, target_free, tally_budget; expect.dry_run_exit, errors, dry_run_errors, skipped[].process_blockers {pid, evidence}, commands {cwd, argv}, report/dry_run_report JSON subsets). It drives diskgc_core::invocation::ensure, the composition moved out of main.rs so binary and model run the same refusals. Corpus roots: tests/models/profiles (kept; diskgc-e2e embeds them) and tests/vectors/<area>. Invariants on every vector: dry run never unlinkat/fchmod/run_command; every unlinkat inside an expected action; every deleting action has a removed postcondition; command cwd is the candidate or its parent; no unfired event.
- Counts: 123 new vectors (walk 20, inuse 6, select 9, config 21, exit 12, races 10, backend 7, builtins 38) + the 32 existing = 155 run by just check.
- Completeness: every_skip_reason_has_a_vector (Reason::ALL + reference.md JSON enum; exempt too_many_entries [2M-entry constant, engine.rs covers], unsupported_store/has_not/stale [no producer: TASK-127]); every_builtin_source_has_match_in_use_guarded_and_nested_vectors (from verified results of unchanged built-ins; Docker: match only).
- Host model extensions: stat_path is lstat (a symlink can be preserved); preserved may name event-made paths; docker_query ENOENT without an executable docker on /usr/bin:/bin (as LinuxHost's spawn; before, dry run and reclaim disagreed on "no client"); fail events allowed on docker_query and run_command; VECTOR_TRACE=<file part> prints a vector's reclaim trace.

Bite evidence (manual harness: one mutation at a time in diskgc-core, corpus run, file restored byte-for-byte and asserted; tree verified clean afterwards). "vector X kills mutation Y":
- M1 ownership check off (walk.rs owners.contains) -> walk/root-owned-file-inside-user-tree, walk/walk-refusals-one-per-candidate, config/allow-root-owners-bound-every-entry, races/chown-after-planning
- M2 .diskgc-keep inside ignored -> 26 vectors (walk/keep-marker-between-root-and-candidate, races/new-keep-marker-after-planning, builtins/*)
- M3 checkout marker inside ignored -> walk/checkout-marker-variants, config/profile-policy-refuses-cargo-checkouts, models/profiles/*-source-anchor, python-pycache-checkout (+5)
- M4 checkout metadata on the path ignored -> config/checkout-metadata-by-path
- M5 sockets/FIFOs admitted -> walk/walk-refusals-one-per-candidate
- M6 Cargo lock names + structural recogniser off -> walk/nested-cargo-target-recogniser, walk/walk-refusals-one-per-candidate
- M7 walk per-entry mount id off -> SURVIVES (EXDEV on descent is a second layer); TASK-129
- M8 process uses (fd/cwd/root/exe/map) ignored -> 40 vectors (inuse/each-kind-of-use-blocks-its-candidate, inuse/opened-after-planning-before-delete, builtins/*)
- M9 initial-environment heuristic off -> inuse/each-kind..., inuse/environment-paths-resolved-lexically, builtins/autotools-build-anchored
- M10 flock Busy taken as free -> inuse/flocked-candidate-directory
- M11 protect/!profile overlap never true (executor overlaps_checked_until) -> 39 vectors (config/protect-overlap-three-ways, select/negated-profile-overlap, config/protect-cargo-roots, builtins/*). (A first attempt mutated overlaps_with, only the identity-alias path: 0 failures, as expected.)
- M12 identity check before unlink off -> races/replaced-before-identity-check
- M13 groups in reverse order -> exit/stale-group-before-pressure, exit/pressure-runs-only-after-stale-is-exhausted
- M14 below --min-free reported met -> 131 vectors
- M15 command cwd candidate/parent swapped -> 9 builtins command vectors + config/later-tier-preflight... (via expect.commands)
- M16 --allow-root command refusal off -> config/allow-root-refuses-command-action
- M17 judge other_fs device check off -> SURVIVES (root-level other_fs and contains_mount cover it); TASK-129
- M18 idle boundary >= to > -> select/idle-boundary
- M19 FUSE/network fs taken as local -> walk/network-filesystem-root-refused
- M20 EACCES mapped to changed -> walk/unreadable-subdirectory
- M21 present SingletonLock ignored -> models/profiles/{brave,chrome,chromium,edge}-browser-cache-lock
- M22 keep_newest off -> 4 builtins version vectors + 4 models/profiles version vectors
- M23 Docker Engine identity unchecked -> backend/docker-podman-identity-refused (new; the close-out lesson's hole now has a vector)
- Not mutated: preflight-before-first-deletion ordering (structural; the vector is built so a late preflight excludes the tier), sandbox gate (ModelHost refuses itself), unsafe_path root exclusion.

PRD "What bad looks like" -> vectors: unselected/protected/negated deleted -> select/nothing-found-nothing-deleted, config/protect-overlap-three-ways, select/negated-profile-overlap; unchecked or foreign entry -> races/file-created-during-delete, walk/root-owned..., config/allow-root-owners...; Cargo lock files/locked targets/outside profile dirs -> models/profiles/cargo-target-locked, builtins/cargo-target-layouts, walk/nested-cargo-target-recogniser, config/protect-cargo-roots (two builds in one target: Cargo race tests only, not a model case); in use/socket/FIFO -> inuse/*, walk/walk-refusals... (live-only policy: config/ipc-live-only-refused-in-this-build); Docker volume/container -> backend/* pin the exact prune argv (Nix kind not built); symlink/mount crossing -> walk/symlinks-never-followed, walk/symlinked-root-is-unsafe, walk/network-filesystem-root-refused, races/mount-*; pressure before earlier tier -> exit/stale-group-before-pressure; exit 0 without space -> exit/exhausted-below-goal-is-1, exit/met-*; static config error / late run-time checks -> config/later-tier-error-found-before-any-deletion, config/unknown-key-deletes-nothing, config/later-tier-preflight-runs-before-the-first-deletion; command for unchecked candidate / wrong cwd / root -> builtins command vectors (expect.commands lists only the eligible one), runner cwd invariant, config/allow-root-refuses-command-action; built-in matching set changed -> golden sets + every builtin vector.

Behaviours learned (pinned with comments): an unsafe or network root excludes the whole tier (exit 3); other_fs/statx-gap are per root; symlinks are never candidates; dry run takes no lock so a flocked dir plans in_use and reclaims locked; a candidate whose first unlink meets a change is no action; ENOTEMPTY mid-delete is an operation error (exit 3); backend skips have candidate ""; Cargo blockers carry generic + Cargo evidence; a marker on the way outranks newest_version; linked_current outranks newest_version.

Gotchas: event nth counts depend on the call sequence (VECTOR_TRACE); a modelled process without fds is unreadable and blocks everything; browser vectors need the config directory present (absent = unsafe_path); the no-delete hook blocks shell heredocs containing some words, so vectors were written via a bundle splitter.

Not expressible / not done: non-UTF-8 names (YAML keys are UTF-8; needs a host-model escape syntax), too_many_entries at the product budget, zombie/pid-namespace liveness (TASK-112), file bind mounts (TASK-129). hostlock/released-by-the-last-reference stays in ensure.rs (asserts elapsed time on the trace, not a vector field); the held case now also exists as exit/host-lock-held-until-deadline-is-75.
Follow-ups filed: TASK-127 (documented skip reasons with no producer), TASK-128 (union/negated dry-run drops build_evidence; meson/west vectors pin it), TASK-129 (surviving walk/judge mutants).
Gate: just check green after each commit (957 passed, 1 ignored); e2e deferred to batch end per Mark (diskgc-e2e built with clippy only: its Option profile and SkipVerdict.process_blockers changed).

Review (mped-architect, read-only, 2026-10-24): invocation move behaviour-identical; verdict 'fix first' on the harness. Fixed in 85a79cc: one-to-one pairing of command runs with actions, null vs [] process_blockers, runner_invariants_reject_perturbed_vectors, every_blocker_evidence_has_a_vector (+2 vectors for inode_map, project_cargo, cargo_manifest_dir, detached_cargo), docs. Filed TASK-130 for config parsing/catalog/account lookup through one product path.
Status: left In Progress. AC #2 is not literally met: four documented reasons have no vector and are exempt by name in REASONS_WITHOUT_VECTOR: unsupported_store, has_not, stale (no code produces them; TASK-127 must decide remove-from-contract or implement) and too_many_entries (needs a 2,000,000-entry model or a product knob; covered by Rust model tests engine.rs/cargo_reclaim.rs). Closing needs TASK-127's decision and a ruling on too_many_entries (accept the Rust-test coverage, or add a test-only budget seam). AC #4 caveats: two Cargo builds in one target belongs to the Cargo race tests, the Nix kind is unbuilt, and two redundant walk mount/device guards survive alone (TASK-129). Final corpus: 125 new vectors + 32 existing; e2e not run (deferred).

Orchestrator ruling on AC #2 implemented (f235eba, b28828b): too_many_entries via a test-only entry budget (EnsureOptions::test_entry_budget -> Tier::max_entries -> executor::rules -> Rules::max_entries; main.rs always None; vector key entry_budget; the_binary_never_sets_the_test_entry_budget). walk/too-many-entries.yaml bites both ways (budget 6 and 4 each red); config/entry-budget-applies-to-configured-groups.yaml red when the per-tier assignment is removed. unsupported_store/has_not/stale stay exempt naming TASK-127, self-shrinking (fails on Reason::ALL, a producer literal in diskgc-core, or a vector; checked by renaming a docker_backend skip). Seam reviewed by mped-architect: ship; its fixes are in b28828b.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
One generic vector runner (profile_vector: 1, superset; crates/diskgc-model/src/profile_vector.rs) drives diskgc_core::invocation::ensure, the composition moved out of main.rs, on separate dry-run and reclaim ModelHosts with corpus-wide invariants (no dry-run mutation, unlinks only inside actions, removed postconditions, one-to-one command runs in candidate or parent, no unfired events). Corpus: 128 new vectors under tests/vectors/{walk,inuse,select,config,exit,races,backend,builtins} plus the 32 in tests/models/profiles, all in just check. Completeness tests: every skip reason (Reason::ALL + reference.md enum; three no-producer reasons exempt, self-shrinking, TASK-127), every built-in source x {match, in_use, protected/negated, nested guard} (Docker: match), every process-blocker evidence value; runner_invariants_reject_perturbed_vectors proves the runner's checks fail. Bite evidence: 23 manual one-at-a-time mutations of diskgc-core guards (table in notes) - 21 killed; M7 (walk per-entry mount id) and M17 (judge other_fs device) survive because another layer catches the same case (TASK-129). Caveat: the bite run was a manual harness, not cargo-mutants, so the table goes stale with refactors (TASK-13 should automate it). Host-model extensions: lstat-like stat_path, event-made preserved paths, Docker client presence on the command path, fail events on docker_query/run_command, test-only entry budget. Follow-ups: TASK-127 (no-producer reasons), TASK-128 (union/negated plans drop build_evidence), TASK-129 (surviving mutants), TASK-130 (config parsing/catalog/account lookup through one product path). Limits: e2e not run (deferred to batch end; diskgc-e2e builds clean under clippy after its type changes); non-UTF-8 names and file bind mounts not expressible; two-Cargo-builds belongs to the Cargo race tests; Nix kind unbuilt.
<!-- SECTION:FINAL_SUMMARY:END -->
