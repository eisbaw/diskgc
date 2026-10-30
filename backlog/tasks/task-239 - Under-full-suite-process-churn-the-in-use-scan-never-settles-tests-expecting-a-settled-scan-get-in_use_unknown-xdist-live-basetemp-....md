---
id: TASK-239
title: >-
  Under full-suite process churn the in-use scan never settles: tests expecting
  a settled scan get in_use_unknown (xdist, live basetemp, ...)
status: To Do
assignee: []
created_date: 2026-10-25 22:15
updated_date: '2026-10-30T22:29:00Z'
labels:
  - test
  - flaky
  - release
dependencies: []
references:
  - crates/diskgc-linux/src/host.rs
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
RC QA on a11e77b: pytest_orphan_xdist_worker_keeps_session - the in_use verdict while the orphan lived is correct; the reclaim after the worker exited got in_use_unknown 'process list still growing after 16 listings' (fail-safe, no deletion). Solo PASS. Investigate whether suite load (other containers forking) keeps the scan unsettled for the whole retry budget, and whether the test should wait for quiescence or the product should settle differently. Logs: tmp/e2e-logs-batch/rc2-a11e77b/.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The live-host holder test explicitly validates InUseUnknown for blocked scans and InUse with holder attribution for settled scans; its blocked branch is reachable.
- [ ] #2 Add deterministic regression coverage for both verdict branches without weakening the production fail-closed rule, and preserve raw diagnostics when investigating the broader suite-churn failures.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Orchestrator 2026-10-25: widened to the class. Also pytest_live_basetemp_session_is_kept failed the same way on release-0.1 af3d503 (load 17-22). Product fails closed (no deletion), but on a host with sustained process churn reclaim may do nothing: investigate both the test (wait for quiescence or isolate the suite's process churn) and the product (settling rule under churn).

2026-10-30: AC1 done in ab891e2 (test-only; just check exit 0, 1245 passed). AC2: the deterministic blocked-branch coverage already exists (diskgc-model tests/inuse.rs only_listings_with_new_pids_count_against_the_bound); the broader suite-churn investigation (container tests under full-suite load) remains open.
<!-- SECTION:NOTES:END -->

## Comments

<!-- COMMENTS:BEGIN -->
created: 2026-10-26T23:15:00Z

---

2026-10-27 project review: another concrete gate failure in this class, with an identified test defect.

Command: nix develop -c just check on working tree based on dd9ece0 (existing task-only changes); exit 101. Log: tmp/review-check.log. Formatting, 65 Python tests, doc checks, parser seed test and workspace Clippy passed. The diskgc-linux unit suite ended 171 passed, 1 failed, 1 ignored; later suites and release-catalog checks were not reached. No rerun was used to claim this fixed.

Failure: host::tests::the_in_use_check_finds_a_holder_on_this_machine, crates/diskgc-linux/src/host.rs. The scan read 661 processes, saw 27 gone, used 17 listings, and took 2.499875127 seconds. It reported an unsettled process list after 16 growing/replacing listings (3 new incarnations in the last listing). scan.check correctly returned Reason::InUseUnknown.

Exact test defect: the unconditional assert_eq!(skip.reason, Reason::InUse) occurs BEFORE if let Some(why) = scan.blocked(). The following branch explicitly intends to handle an unsettled scan, but the assertion prevents reaching it. Move the reason assertions into the corresponding branches: InUseUnknown for the blocked scan, InUse plus holder attribution for the settled scan. Keep checking that blocked scans refuse candidates; do not weaken the product's fail-closed behavior. Retain this task's broader investigation of container-suite churn separately from this deterministic assertion mismatch.

---
<!-- COMMENTS:END -->
