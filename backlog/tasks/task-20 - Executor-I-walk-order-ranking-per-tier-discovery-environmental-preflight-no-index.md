---
id: TASK-20
title: >-
  Executor I: walk order, ranking, per-tier discovery, environmental preflight
  (no index)
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-18 13:16'
labels:
  - wave-2
  - irreversible
  - safety
dependencies:
  - TASK-19
references:
  - docs/architecture.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the walk decides order; policy order is a core promise. Extend the executor slice from the minimal ensure: groups in order, ranking ordered/random with seed, per-tier discovery before leaving a tier, environmental preflight of all tiers before the first deletion, misconfigured tiers excluded while others run, stop at the observed goal. Deadlines and signals are the Executor II task.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A pressure tier never runs while an earlier tier has discoverable eligible candidates (model test)
- [ ] #2 A misconfigured tier is excluded and reported while other tiers still run
- [ ] #3 Trace assertion: every tier's environmental preflight runs before the first deletion
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-5 (deletion engine): engine outcomes for the executor: a refused candidate (WalkStop::Skipped) never touched anything; End::Changed/Failed may be partial (removed_entries > 0), so re-sample space after every candidate whatever the outcome; End::GateRefused must stop the run (never retried, never counted as a skip). Deadlines: pass the run/tier deadline to both walk and delete; they check it before every statx/listing and every unlink (cooperative; a blocked syscall overruns).

From task-5's review: the walk returns WalkStop::Failed for process-level errors (EMFILE, ENFILE, ENOMEM, EIO, ...) and delete returns End::Failed / End::GateRefused; the executor must treat these as run errors, not candidate skips (user decision 2026-10-17).

From task-6 (in-use check): call diskgc_core::inuse::delete_unused(host, &mut checker, &candidate, &rules, options) for each fs candidate: it locks the candidate dir (flock exclusive, non-blocking; busy = skip locked), walks under the lock, checks the locked dir is the walked one (else changed), runs the in-use check and deletes while the check is fresh (re-checks once with a new scan if the 1 s bound passed before the delete would start). Keep ONE inuse::Checker per run (REUSE_LIMIT 500 ms: a scan serves later candidates checked within half a second). Results: Ok(Deletion) as engine::delete; Err(Stop::Skipped(skip)) (in_use, locked, contains_ipc, changed and every walk skip); Err(Stop::Walk(WalkStop::DeadlinePassed|Failed)) and Err(Stop::Failed(HostError)) are run errors (Stop::Failed includes: cannot list /proc, a scan slower than the freshness bound twice, open/flock errors). Log checker.latest().notes() once per scan (unreadable processes and why each does or does not block; Failed environments; an unsettled process list). held_by_deleted_files: Scan::held_by_deleted_files(dev of the requested filesystem) from the last scan, or a fresh inuse::scan at report time. docs/testing.md's bucket scenario now says: the later member is in_use when the scan used for the earlier one is older than the reuse limit or a suspend intervened; an open inside the reuse window is the accepted limit 5 (pinned by crates/diskgc-model/tests/inuse.rs a_scan_is_not_reused_across_a_suspend). If the user wants every member scanned afresh, construct the Checker with Duration::ZERO (about 0.2 s per candidate on a busy desktop).

From task-6 round 2 (supersedes parts of the earlier task-6 note): build the checker with inuse::Checker::new(inuse::Options::from_rules(&rules), inuse::REUSE_LIMIT); delete_unused refuses (Stop::Failed) options that disagree with the rules. Freshness now lives in Checker::check: a clear is returned only if its scan is fresh after the verdict (otherwise one new scan; two stale scans = error); start engine::delete right after it, never after Clear::fresh_until. Skips (in_use etc.) need no freshness.

From task-6 round 3: inuse::Checker::new now takes &Rules (Checker::new(&rules, inuse::REUSE_LIMIT)); delete_unused deletes through inuse::delete_cleared, which refuses a Clear past its fresh_until (the step then checks once more). Clear has no public constructor.

SUPERSEDED: the round-2 task-6 note above ('From task-6 round 2 ...: build the checker with inuse::Checker::new(inuse::Options::from_rules(&rules), ...); ... start engine::delete right after it') is stale. Current API: Checker::new(&rules, REUSE_LIMIT); delete through inuse::delete_cleared (Clear is bound to the walk record's root identity, not Copy, refused when expired or for another record); see the round-3 note and task-53's.

From the task-7 review (H9): a union selects a candidate if any of its terms does. Deduplicate candidates found by several terms (one plan/action entry each), and report a filter skip (not_idle, has_not) only when no term selects the candidate.

From task-53: the cargo-target action is cargo_target::reclaim (see task-10's note for the call and the ReclaimEnd mapping). For the walk order: Eligible::last_build (eligibility, no locks) is the 'oldest' key and is passed back as judged_last_build so a target rebuilt between ordering and acting is changed. reclaim is one target at a time and holds every lock of that target for its whole walk+scan+delete (measured 0.49 s per 100k entries untraced on tmpfs); deadlines are checked between operations, so a tier deadline stops it part-way (Stopped(DeadlinePassed), fingerprints already gone first). GateRefused stops the run.

From task-10: the executor slice is diskgc-core/src/executor.rs (ensure<H: Delete>, dry_run<H: Host>, Tier, Request, privileges) with report/mod.rs (Status, conclude for rows 0/124/3/1, to_json, summary). Today: one --select tier (Tier::select refuses unions, !filter:, !profile:, non-cargo-target kinds), discovery + a read-only cargo_target::tally per target AFTER the host lock (architecture.md notes the deviation from the pre-lock synchronous tally), sort by tally last_build (unknown last), reclaim each with judged_last_build None, re-sample after each, stop when avail >= --target-free; after the lock a fresh sample >= --min-free returns met before any discovery. Extend Reclaim::walk to groups/ranking/per-tier discovery/preflight; keep ReclaimEnd mapping (Complete=action; Skipped=skip, plus an action when removed_entries>0; Failed=candidate error, next target tried; GateRefused and Stopped(Failed)=run error, stop; Stopped(DeadlinePassed)=deadline_hit). The dry run plan is eligible-first in the same order (reference.md says so); keep dry run and ensure on the same ordering function. held_by_deleted_files comes from the last scan, or one made at the end.

From task-68's review (mped S2): this build refuses a reclaiming ensure over a non-Cargo source at the top of executor::ensure (an allowlist on Tier.source), and executor::discover matches Tier.source exhaustively so an fs tier never reaches Cargo discovery or reclaim (model test an_fs_tier_never_reaches_the_cargo_reclaim_over_its_roots). With config tiers or multi-term selections, the kind refusal (a kind with no reclaim yet) must become a static check over EVERY tier, made before any tier deletes: refusing tier 3 after tiers 1-2 deleted breaks "a static config error discovered only after something was deleted" (PRD, What bad looks like).

From task-68 r2 (mped N3): executor::discover's fs arm is unreachable through ensure (the allowlist guard runs first) and only bites under mutation; add a crate-internal test calling discover with an fs tier, or give Reclaim a cargo-only tier type so the arm cannot exist. Cosmetic: unwrap_or_default() puts an empty path in that error when roots are empty.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
