---
id: TASK-239
title: >-
  Under full-suite process churn the in-use scan never settles: tests expecting
  a settled scan get in_use_unknown (xdist, live basetemp, ...)
status: To Do
assignee: []
created_date: '2026-10-25 22:15'
updated_date: '2026-10-25 22:54'
labels:
  - test
  - flaky
  - release
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
RC QA on a11e77b: pytest_orphan_xdist_worker_keeps_session - the in_use verdict while the orphan lived is correct; the reclaim after the worker exited got in_use_unknown 'process list still growing after 16 listings' (fail-safe, no deletion). Solo PASS. Investigate whether suite load (other containers forking) keeps the scan unsettled for the whole retry budget, and whether the test should wait for quiescence or the product should settle differently. Logs: tmp/e2e-logs-batch/rc2-a11e77b/.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Orchestrator 2026-10-25: widened to the class. Also pytest_live_basetemp_session_is_kept failed the same way on release-0.1 af3d503 (load 17-22). Product fails closed (no deletion), but on a host with sustained process churn reclaim may do nothing: investigate both the test (wait for quiescence or isolate the suite's process churn) and the product (settling rule under churn).
<!-- SECTION:NOTES:END -->
