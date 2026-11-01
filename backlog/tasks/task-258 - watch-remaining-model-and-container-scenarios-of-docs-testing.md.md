---
id: TASK-258
title: 'watch: remaining model and container scenarios of docs/testing.md'
status: To Do
assignee: []
created_date: '2026-10-31T00:59:00Z'
updated_date: '2026-11-01T04:10:00Z'
labels:
  - watch
  - testing
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-32 (implemented in diskgc-model tests/watch.rs and diskgc-e2e watch_suite.rs). Still open: model watch_cycle_deadline (a tier slower than --timeout: deadline record, no deletion after it, a running child gets SIGTERM); watch_root_vanished_after_start_still_reclaims_other_tiers; watch_signals for SIGINT and SIGHUP (ModelSignal has no Hup yet) and for a signal during the host-lock wait; watch_retry_back_off with a Docker tier once a backend reports backend_unavailable (the doubling is tested with a stacked mount). Container: stdout closed by the reader exits 3 (watch_process_reclaims_and_streams_json's second half), watch_process_stops_on_each_signal incl. SIGHUP under nohup, watch_and_ensure_share_a_filesystem (/proc/locks sampled), background_tally_does_not_load_the_system (needs watch --tally-only, now available).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
t272 branch aa5fa25: model watch_cycle_deadline_starts_no_deletion_after_it, watch_root_vanished_after_start_still_reclaims_other_tiers, watch_sigint_in_a_cycle_..., watch_sigterm_during_the_host_lock_wait_stops_at_once. Still open: SIGHUP (not modelled), watch's own child-at-deadline container case, watch_retry_back_off with Docker, watch_cycle_beside_ensure (holder frees enough), container watch_process_reclaims_and_streams_json second half, watch_process_stops_on_each_signal, watch_and_ensure_share_a_filesystem, background_tally_does_not_load_the_system.
<!-- SECTION:NOTES:END -->
