---
id: TASK-257
title: >-
  watch: a second signal during the stop skips the snapshot write and the
  child's grace
status: Done
assignee: []
created_date: '2026-10-31T00:59:00Z'
updated_date: '2026-11-01T05:25:00Z'
labels:
  - watch
  - contract
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-32: docs/reference.md (Watch signals) says a second SIGTERM, SIGINT or SIGHUP during the stop skips the snapshot write (its O_TMPFILE never linked in) and the rest of a running child's 10 s SIGTERM grace, and exits 0 at once. This build records only the first signal (LinuxHost's INTERRUPTED flag), so the stop always writes the snapshot. Needs a signal count on the Host port (ModelHost events too) and the watch loop/Writer checking it right before the write.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A second signal during the stop's snapshot write leaves the old snapshot unchanged and exits 0 (model test, ModelHost signal events)
- [ ] #2 A second signal during a cycle's child grace exits 0 without waiting out the grace, the child keeps the host lock it inherited
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
t272 branch 4be0390: Host::second_signal (LinuxHost counts under watch's handlers only), state_write -> StateWritten::Cancelled before the link, watch stop skips the write, LinuxHost run_command leaves the child running on a second signal during its grace. Model tests for AC1, container watch_second_signal_leaves_a_stubborn_child_holding_the_host_lock for AC2 (PASS, bite FAIL with second_signal forced false). Awaiting review round.

2026-11-01: merged in the watch contract batch after one review round (verification note on the merge).
<!-- SECTION:NOTES:END -->
