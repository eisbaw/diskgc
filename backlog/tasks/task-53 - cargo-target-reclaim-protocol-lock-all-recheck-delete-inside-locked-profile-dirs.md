---
id: TASK-53
title: >-
  cargo-target reclaim protocol: lock all, recheck, delete inside locked profile
  dirs
status: To Do
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-18 01:32'
labels:
  - wave-1
  - irreversible
  - safety
dependencies:
  - TASK-9
references:
  - docs/safety.md#cargo-target
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: split from the Cargo kind task; this is the step that deletes. Implement the reclaim protocol in docs/safety.md#reclaim-protocol: take every known lock without waiting, re-list and recheck (last build, ownership walk, process checks), delete .fingerprint first then all but lock files inside each locked profile dir with the per-entry identity check, release.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No lock file is ever unlinked or replaced (deletion oracle by inode plus strace record)
- [ ] #2 target/doc, target/tmp, target/criterion and CACHEDIR.TAG are never touched
- [ ] #3 A new lock file or a newer build appearing between discovery and locking yields changed (model test with an event on the call trace)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-14: the testing.md sketch (crates/diskgc-model/tests/models/sketch/locked-cargo-target.yaml, expect exit 1 and skipped locked) runs through a stand-in Cargo lock check in crates/diskgc-model/tests/sketch.rs. Once this protocol and the executor exist, run the fixture through them and delete the stand-in.

From task-51: the race tests drive single operations through diskgc-e2e-driver (diskgc-e2e/src/driver.rs); add this task's operation (Cargo discovery / reclaim protocol) as a subcommand in driver::run, generic over H: Delete so its unit tests run on ModelHost, with indexed result lines. Every port call it makes is countable by the controlled wrapper (pause/kill points).

From task-5 (deletion engine): delete inside each locked profile directory by running engine::walk + engine::delete per entry (a Candidate is an open parent fd, its mount id and the entry's path): .fingerprint/ first, then every other entry except the lock files (never make a lock file a candidate). The recogniser only looks inside a candidate, so entries of a profile directory are not contains_special. The engine stops at the first identity mismatch (End::Changed) and reports End::Failed { ENOTEMPTY } when Cargo created something after the walk; End::GateRefused is a run error. Call positions are reproducible (names sorted per directory), which race test 6's seeded kill positions can rely on.

From task-5 round 2 (deep gate): the engine now refuses (contains_special) any candidate holding an entry with a known Cargo lock name (.cargo-lock, .cargo-build-lock, .cargo-artifact-lock) anywhere, the candidate itself included, so a profile directory can never be a plain-delete candidate. This task's plan is unaffected: its candidates are the entries of a locked profile directory except the lock files (.fingerprint/, deps/, build/, incremental/, binaries), which hold no lock files in the layouts measured (Cargo 1.90 and 1.96 put them at the profile root). If one ever does, the engine refuses that entry (contains_special), which is the conservative outcome (treat as unsupported_layout here). Candidate::new now takes the parent's &Resolved plus the entry's path.

From task-6: the Cargo protocol holds the Cargo lock files, not a flock on each entry, so do not use inuse::delete_unused for the entries of a locked profile dir; use inuse::Checker::check(host, &candidate, &record) after the walk of each entry and start engine::delete before the returned Clear.fresh_until (re-check with a new scan, checker.forget(), when it passed). The Cargo-specific process heuristics (task-9) run on the same scan. A suspend between check and delete is handled by fresh_until (CLOCK_BOOTTIME).

From task-6 round 2: Checker::check now re-checks against a new scan itself when the scan is stale after the verdict; call it right before each engine::delete and start the delete at once (Clear::fresh_until is the hard limit). Build the Checker with Options::from_rules of the entry walk's Rules.

From task-6 round 3 (SUPERSEDES the two earlier task-6 notes on this task, which contradicted each other on freshness): the API is settled. Build one checker with inuse::Checker::new(&rules, inuse::REUSE_LIMIT) from the Rules of the entries' walks. For each entry of a locked profile dir: engine::walk, then checker.check(host, &candidate, &record) -> Result<Result<Clear, Skip>, HostError> (a Clear is made only there and only if its scan is fresh after the verdict; the checker itself rescans once otherwise), then inuse::delete_cleared(host, clear, &candidate, &record, options) -> Result<Deletion, Clear>, which refuses an expired Clear (Err: call checker.forget() and check again, once). Do not call engine::delete directly for checked candidates; delete_cleared is the deletion entry point.

From task-6 round 4 (amends the round-3 note): inuse::delete_cleared now returns Result<Deletion, NotCleared>: NotCleared::Expired(Clear) (forget and check again, once) or NotCleared::OtherCandidate (the Clear was made for another walk record: a bug). Clear is not Copy or Clone and carries the record root's identity.

CORRECTION (task-6 final review, mped and Codex): NotCleared::OtherCandidate compares only the walk root's identity. It does NOT catch walking the same object again between check and delete, nor a hard-linked file candidate. Always delete with the exact record that was checked.

From task-4: Cargo 1.99.1 and the 2026-10-16 nightly hold .cargo-lock shared (not exclusive) during a build, with .cargo-build-lock and .cargo-artifact-lock exclusive (docs/safety.md, Lock files). Step 1's exclusive non-blocking flock of every known lock fails against a shared holder too, so the protocol is unaffected; a model test of 'locked' should include a shared holder.

From the task-4 review (mped, reproduced by task-4; docs/safety.md, Lock files and Known gaps): nightly-2026-10-16 uses Cargo's new build-dir layout by default: the profile directory has no .fingerprint/ and no deps/; each unit gets build/<package>/<hash>/{fingerprint,out}; lock files as 1.99.1. Reclaim step 3's 'delete .fingerprint/ first' and the Last build measure (.fingerprint/, deps/) do not work on that layout; revise both (e.g. the per-unit fingerprint directories under build/ first, and Last build over build/ and incremental/). Also: with CARGO_BUILD_BUILD_DIR, 1.99.1 and the nightly split the lock set between target/<profile>/ (.cargo-artifact-lock exclusive, .cargo-lock shared) and the build-dir's <profile>/ (.cargo-build-lock), each half with its own CACHEDIR.TAG, so each half is a target of its own for discovery and locking.

From task-9 (cargo_target in diskgc-core; rules in docs/safety.md cargo-target, which task-9 rewrote): what the protocol gets.
- Input: cargo_target::Target from discover(): root, path, identity, inspected (Inspected: profiles = ProfileDir {rel, identity, changed, locks: Vec<LockFile {name, identity, changed}> sorted, entries: other names sorted}, dirs directly in the target, keep_marker). Lock files to take = every profile's locks (all known: judge_layout refuses unknown names first).
- Reuse, do not re-implement: reach(host, &target, deadline) -> Reached {target: Resolved (O_PATH), project: Option<DevIno>} (re-walks from the root, keep markers on the way, identity check); inspect(host, &fd, mount_id, path, deadline) for step 2's re-listing and Inspected::same_locks(&target.inspected) for 'a new lock file / profile since discovery' (changed; race test 5); judge_layout(path, &inspected) (unknown lock name, split halves, unmeasured sets, unknown layout -> unsupported_layout; empty); walk_profiles(host, &reached.target, path, &inspected, &rules, deadline) -> Vec<ProfileWalk> (each entry but the lock files walked as its own engine Candidate; ProfileWalk::candidates()); last_build(&inspected, &walks) + IdleFor::verdict(last, host.wall_clock()); cargo_evidence(scan, path, target.identity.id, &inspected, reached.project) for the Cargo process rules (non-empty = in_use). eligibility() shows the composition without locks; the protocol is the same with step 1 first and inuse::Checker::check (Clear) + inuse::delete_cleared per entry instead of Checker::current.
- Step 1 locking: open each lock below the profile dir with open_beneath(Access::Read, Want::Any), check the fd's identity == LockFile.identity (a replaced lock file is changed), flock_try Exclusive. Lock files are in Inspected::objects(), so another process holding one open is rule-1 evidence; diskgc's own pid is never counted.
- Split pairs never reach the protocol (both halves unsupported_layout, decision recorded in safety.md Lock files).
- Fingerprint first, per cargo_target::Layout: Classic -> the entry .fingerprint first; PerUnit -> every build/<pkg>/<hash>/fingerprint directory first (find them in the `build` entry's walk record), then the rest. Suggestion: delete the layout marker last (Classic: deps; PerUnit: build), so a run killed part-way leaves a profile still in a known layout; a profile with none of .fingerprint/, deps/, build/ is unsupported_layout and would otherwise stay unreclaimed until the next cargo build. safety.md 'Known gap, the per-unit layout and the protocol' is yours to close.

From task-9 round 2 (review filings H1, H4, H6):
- H1: the build directory's half of a split pair alone might be reclaimable while holding its .cargo-build-lock, if Cargo always takes it before touching the build dir and the test-binary window is covered (detached rule 6 covers a build dir outside the project while any cargo runs). Measure first (task-11 AC #4); until then both halves are unsupported_layout.
- H4: a per-target entry budget: walk_profiles gives each profile entry its own DEFAULT_MAX_ENTRIES budget, so one target can hold several million entries in records at once (memory). Consider one budget per target for the protocol.
- H6: (a) a lock file created mid-protocol (after step 1, before step 3): step 2 must re-inspect and compare with same_locks under the held locks, and the deletion of each entry must not run after a new lock appears (re-check before each profile dir?); (b) profile directories are opened twice (inspect, then walk_profiles reopens and checks identity): keep one fd per profile if the protocol needs the same object throughout; (c) how long locks are held: the whole profile's walk, scan and delete run under every lock of the target; a big target blocks Cargo (prints Blocking waiting for file lock) for that long: bound or report it.
- Reached.project is now Project {id, detached}; cargo_evidence takes it.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
