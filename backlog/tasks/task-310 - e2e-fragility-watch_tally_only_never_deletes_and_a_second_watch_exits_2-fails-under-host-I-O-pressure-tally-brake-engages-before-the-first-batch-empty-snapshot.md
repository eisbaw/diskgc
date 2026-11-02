---
id: TASK-310
title: >-
  e2e fragility: watch_tally_only_never_deletes_and_a_second_watch_exits_2 fails
  under host I/O pressure (tally brake engages before the first batch, empty
  snapshot)
status: To Do
assignee: []
created_date: '2026-11-02T00:42:00Z'
labels:
  - test
  - flaky
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the TASK-304 implementer (2026-11-02): under host I/O pressure (PSI 23-44%) the background tally's pressure brake engages before the first batch, so the snapshot is empty and the test fails 245/246; master's code fails the same way at the same pressure; it passed when quiet. Log: tmp/worktrees/t304/tmp/t304-logs/e2eR-watch-tally-fail.log (copied to tmp/evidence-t304/). Fix the test, not the brake: drive the tally with an explicit low-pressure condition or assert 'snapshot written or tally paused for pressure' with the reason logged; never weaken the pressure brake.
<!-- SECTION:DESCRIPTION:END -->
