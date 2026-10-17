---
id: TASK-20
title: >-
  Executor I: walk order, ranking, per-tier discovery, environmental preflight
  (no index)
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 19:31'
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
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
