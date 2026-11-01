---
id: TASK-294
title: >-
  Mount table freshness across deletion steps: reuse only while poll(POLLPRI) on
  /proc/self/mountinfo reports no change
status: To Do
assignee: []
created_date: '2026-11-01T09:06:00Z'
labels:
  - safety
  - performance
  - needs-mark
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the TASK-292 review (mped, 2026-11-01). After TASK-292 the reclaim main thread spends ~30% re-reading /proc/self/mountinfo, which safety.md's Deletion protocol reads afresh at several steps (step 1 'read now', step 4 'read again', before the delete). Sharing one table ACROSS steps changes the documented protocol, so it is Mark's safety decision. Recommended rule: 'unchanged since read' as told by the kernel, not by time: hold /proc/self/mountinfo open, poll for POLLPRI/POLLERR before reuse, reuse only while a zero-timeout poll reports no change; any event or poll error means read again. Exact for diskgc's own mount namespace (same scope as today); the mechanism libmount's monitor and systemd use. Needs a change-event model in ModelHost and a test where a mount appears between steps. Reject a time-based reuse window (adds a new accepted-limit window). Weaker fallback if declined: share one table across the read-only steps (step 1, tally reach, step 2), keep the read under the lock and the one before the delete separate. Measure first (TASK-274's within-step collapse may be enough).
<!-- SECTION:DESCRIPTION:END -->
