---
id: TASK-239
title: >-
  pytest orphan xdist test: post-exit reclaim ends in_use_unknown (process list
  unsettled) inside the full suite
status: To Do
assignee: []
created_date: '2026-10-25 22:15'
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
