---
id: TASK-32
title: 'watch: thresholds, interval, back-off, --tally-only, signals'
status: Done
assignee: []
created_date: 2026-10-16 22:41
updated_date: '2026-10-31T03:31:00Z'
labels:
  - wave-3
  - cli
dependencies:
  - TASK-31
  - TASK-117
  - TASK-231
references:
  - docs/reference.md#watch
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the laptop use case is a long-running service. Implement watch per docs/reference.md#watch: required --target-free, interval polling, reclaim below --min-free, back-off rules (exhausted/misconfig vs backend errors), --tally-only, SIGTERM exit 0, one log line per back-off.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Between the thresholds watch makes zero deletions (container test over time with the virtual clock in model)
- [x] #2 Back-off timings follow the documented rules (model tests)
- [x] #3 --tally-only never deletes (deletion oracle) and keeps the snapshot fresh
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Orchestrator 2026-10-23 (design audit): now depends on TASK-117 (watch machine interface design).

Design (TASK-117) settled in 2d6c1ba on branch watch-design: implement per docs/reference.md#watch and the watch scenarios in docs/testing.md; unit restart requirement for TASK-119 in architecture.md Deployment.

COMPASS round 4 (2026-10-25): watch must not ship with the known finder-set id exhaustion crash (TASK-231).

2026-10-31: implementation merged to master at bf9e530 after one review round (mped + Codex). Kept In Progress: the three watch container tests and the walk have never run (disk < 15 GiB). Close only after a container walk: start, reclaim to target, quiet between thresholds, SIGTERM exit 0, tally-only oracle clean, second watch exit 2. Follow-ups to file: btrfs watcher exclusion via mountinfo device (Codex 10) with model support for subvolumes; N1 sample numbers in a device-mismatch record; a test that a slow index wait eats into the cycle deadline.

2026-10-31 walked and closed. Container tests on master 12387c1 (runner built from it): watch_process_reclaims_to_target_then_stays_quiet_and_stops_on_sigterm, watch_tally_only_never_deletes_and_a_second_watch_exits_2, watch_tally_pauses_while_another_process_holds_the_host_lock all PASS, oracle clean; fast suite 232/232. Walk (tmp/walks/2026-10-31-watch/): built-in selection on fresh data -> exhausted + hold back-off, SIGTERM exit 0; between thresholds quiet while the tally indexed 5 units; below --min-free the index-served cycle reclaimed 4 of 5 oldest-first to --target-free (first deletion 80 ms) and stayed quiet; one JSON record (index null: TASK-244). Kinks filed separately.
<!-- SECTION:NOTES:END -->
