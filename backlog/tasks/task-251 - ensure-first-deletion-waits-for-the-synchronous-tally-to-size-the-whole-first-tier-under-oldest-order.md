---
id: TASK-251
title: >-
  ensure: first deletion waits for the synchronous tally to size the whole first
  tier under oldest order
status: To Do
assignee: []
created_date: '2026-10-30T22:48:00Z'
updated_date: '2026-11-01T01:02:00Z'
labels:
  - wave-3
  - performance
dependencies:
  - TASK-31
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found walking TASK-31's AC4 e2e (branch t31, 2026-10-30): pass 2 of the synchronous tally sizes every stale single of the first tier before the host lock is taken, and the queue's first deletion comes only after it. Measured (rootless podman, tmpfs /tmp, dev build, under strace): 8,000 entries / 2,000 rc dirs: first deletion 82 ms of which the tally 71 ms (warm 65/54); 100,001 entries / 10,000 rc dirs (2026-10-26): 592/570 ms cold, 389/371 ms warm. Under 'oldest' the order needs no sizes (best-fit does), so on a cold index the time to first deletion is roughly the time to size the tier. The PRD prescribes this pass ('then only the first tier's stale candidates sized'), so this is a design question, not a defect: should 'oldest' tiers skip pass 2 (or size lazily, ahead of the walk) when the goal is far from the whole tier? Related: TASK-245 (reuse the tally's sizing as the read-only tally) removes one of the three walks but not this wait. Not a latency target (Mark 2026-10-25: none); only whether the wait buys anything for 'oldest'.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01 host evidence raises this: /tmp on Mark's laptop holds ~208k entries (68 GiB): nix-shell-rc 140,372 candidates (0.87 GiB, leftovers of every nix develop) + pytest-tmp 6,424 (12.25 GiB). A dry run over both took 418-511 s (default --timeout 10m). A real ensure sizes the whole first tier before the first deletion, so on this host time-to-first-deletion would be minutes; with nix-shell-rc first in a tier, nearly all of it spent on 0.87 GiB of tiny dirs. Measure the real ensure's time-to-first-deletion in a container with this shape (140k small + 6.4k large) and fix (size lazily / oldest-first without full sizing / per-tier budget).
<!-- SECTION:NOTES:END -->
