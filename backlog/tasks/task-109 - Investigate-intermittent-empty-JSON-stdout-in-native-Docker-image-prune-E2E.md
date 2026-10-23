---
id: TASK-109
title: Investigate intermittent empty JSON stdout in native Docker image-prune E2E
status: To Do
assignee: []
created_date: '2026-10-22 07:03'
updated_date: '2026-10-23 15:24'
labels:
  - testing
  - safety
  - wave-2
dependencies: []
references:
  - diskgc-e2e/src/docker_suite.rs
  - diskgc-e2e/src/ensure_suite.rs
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A full nix develop -c just e2e run exited 1 after Docker 6/7: native_docker_image_prune_preserves_containers_volumes observed expected process exit 1 but no JSON stdout. The prior shared report() assertion omitted stderr, so the immediate cause was lost. The isolated test passed, and after improving diagnostics, 20 bounded grouped Docker fast-suite repetitions (7/7 each) passed without reproducing. This is unresolved evidence, not a fixed flake. Next failure should preserve argv, exit, signal, stdout and stderr; inspect the strace record and daemon logs to identify the first failing boundary before changing behavior.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A failing grouped/full run records process stderr, exit, signal, and relevant strace/daemon evidence without hiding the original failure.
- [ ] #2 The exact root cause of missing JSON stdout is identified and fixed at source; a regression demonstrates the failure before the fix and passes after it.
- [ ] #3 The native Docker prune journey and full just e2e gate pass with the fix.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-22: report() now names argv, exit, signal, stdout, stderr. Docker prune assertion also appends bounded raw per-PID strace tails, dockerd process status, and private daemon log tail. Twelve grouped runs passed 7/7 each (84/84), including the target 12/12. A subsequent temporary in-container fault injection cleared captured stdout after a successful real prune; grouped Docker run failed 6/7 and its host log retained the original JSON assertion plus all diagnostic fields. The injection was removed. The actual intermittent failure did not recur. CLI write_result returns exit 3 on stdout write failure, so prior exit 1 with empty stdout is not explained by a normal ensure report path. Root cause remains unproven; do not classify as fixed.

Orchestrator 2026-10-23 (COMPASS consult): parked with a trigger: not reproduced after 20+ grouped runs; keep diagnostics in place and reopen on recurrence (record the raw diagnostics then).
<!-- SECTION:NOTES:END -->
