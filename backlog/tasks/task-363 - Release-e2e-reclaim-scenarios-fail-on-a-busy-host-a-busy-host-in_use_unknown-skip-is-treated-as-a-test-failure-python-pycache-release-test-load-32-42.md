---
id: TASK-363
title: >-
  Release/e2e reclaim scenarios fail on a busy host: a busy-host in_use_unknown
  skip is treated as a test failure (python-pycache release test, load 32-42)
status: In Progress
assignee: []
created_date: '2026-11-06T00:13:00Z'
updated_date: '2026-11-06T05:18:00Z'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-06: recurred twice in TASK-360's full e2e (release_candidate_ccache_* kept ~/.cache/ccache or ~/.ccache in_use_unknown, 'process list still growing or replacing pids after 16 listings'; release_candidate_python_venv_* kept both venvs in_use, hypothesis: another test container in the host PID namespace had a process naming the identical /home/tester/src/pyapp path -> false in_use, a test-isolation flaw). Logs tmp/evidence-t363/. Assigned to the t360 implementer (branch t360-cleaners) as separate commits before the Jest exclusion commit. Root-fix candidates: give release scenarios a deadline long enough for TASK-340's back-off to retry instead of asserting on the first unsettled scan; unique per-test project paths; deterministic busy-host test separate.
<!-- SECTION:NOTES:END -->
