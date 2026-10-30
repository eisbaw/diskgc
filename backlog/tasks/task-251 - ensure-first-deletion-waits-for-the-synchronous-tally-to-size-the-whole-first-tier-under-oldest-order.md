---
id: TASK-251
title: >-
  ensure: first deletion waits for the synchronous tally to size the whole first
  tier under oldest order
status: To Do
assignee: []
created_date: '2026-10-30T22:48:00Z'
labels:
  - wave-3
  - performance
dependencies:
  - TASK-31
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found walking TASK-31's AC4 e2e (branch t31, 2026-10-30): pass 2 of the synchronous tally sizes every stale single of the first tier before the host lock is taken, and the queue's first deletion comes only after it. Measured (rootless podman, tmpfs /tmp, dev build, under strace): 8,000 entries / 2,000 rc dirs: first deletion 82 ms of which the tally 71 ms (warm 65/54); 100,001 entries / 10,000 rc dirs (2026-10-26): 592/570 ms cold, 389/371 ms warm. Under 'oldest' the order needs no sizes (best-fit does), so on a cold index the time to first deletion is roughly the time to size the tier. The PRD prescribes this pass ('then only the first tier's stale candidates sized'), so this is a design question, not a defect: should 'oldest' tiers skip pass 2 (or size lazily, ahead of the walk) when the goal is far from the whole tier? Related: TASK-245 (reuse the tally's sizing as the read-only tally) removes one of the three walks but not this wait. Not a latency target (Mark 2026-10-25: none); only whether the wait buys anything for 'oldest'.
<!-- SECTION:DESCRIPTION:END -->
