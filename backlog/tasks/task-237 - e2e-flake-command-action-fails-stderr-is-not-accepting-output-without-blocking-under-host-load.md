---
id: TASK-237
title: >-
  e2e flake: command action fails 'stderr is not accepting output without
  blocking' under host load
status: To Do
assignee: []
created_date: '2026-10-25 21:56'
labels:
  - e2e
  - flaky
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen once in the fast container suite on t29 (2026-10-25, host load 12-17): npm_cache_cleaner failed, exit 3 instead of 1: '/home/tester/.npm: forward command output /bin/npm: stderr is not accepting output without blocking'. diskgc-linux host.rs forwards a command's output to diskgc's own stderr with a fixed COMMAND_STDERR_BACKPRESSURE of 50 ms; under load the container's stderr reader (conmon / the runner) can lag past it. 3 of 3 reruns alone passed (not a resolution). Not in TASK-29's diff. Logs: tmp/worktrees/t29/tmp/t29-e2e-logs (npm_cache_cleaner), summary tmp/worktrees/t29/tmp/t29-e2e-fast.log. Decide: is turning backpressure on diskgc's own stderr into an action error the right contract (a full or slow terminal fails a reclaim), or should output be dropped/truncated with a note; and make the test not depend on host load.
<!-- SECTION:DESCRIPTION:END -->
