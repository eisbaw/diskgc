---
id: TASK-363
title: >-
  Release/e2e reclaim scenarios fail on a busy host: a busy-host in_use_unknown
  skip is treated as a test failure (python-pycache release test, load 32-42)
status: To Do
assignee: []
created_date: '2026-11-06T00:13:00Z'
labels:
  - flaky
  - e2e
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-359 e2e run 2 (2026-11-06, tree of 4493790e): release_candidate_python_pycache_reclaim_then_a_live_importer_recompiles kept one __pycache__ as in_use_unknown (cause busy) at host load 32-42 from other projects; diskgc behaved as documented (never deletes on an unsettled scan), the test asserted deletion. Run 3 on the same tree passed; a passing rerun does not resolve it. Raw logs: tmp/evidence-tadmit-busy/. Any reclaim scenario that asserts deletion can fail this way under load. Do: decide the test design (e.g. scenarios assert 'reclaimed OR kept busy with the documented reason' plus a separate deterministic busy-host test; or run the scan against a settled process view like OwnProcessOnly where the scenario is not about in-use), related to TASK-317 (load flakes) and TASK-348 (busy-host classification).
<!-- SECTION:DESCRIPTION:END -->
