---
id: TASK-236
title: >-
  watch: run the background tally on its own thread (lower_priority is per
  thread) and wire its container tests
status: To Do
assignee: []
created_date: '2026-10-25 21:56'
labels:
  - tally
  - watch
dependencies:
  - TASK-29
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-29: Host::lower_priority sets nice 19 and the idle I/O class on the CALLING THREAD only (Linux applies both per task), so watch must run the background tally (diskgc-core tally::background) on a thread of its own that starts no reclaim work, or the reclaim inherits idle priority. Also carries TASK-29 AC #1 (container test: the tally pauses while another process holds the host lock) and the background_tally_does_not_load_the_system container test, which need watch --tally-only. Concurrent use of one TallyState by watch's reclaim and its tally needs a review then (mped-architect, TASK-29 round 2).
<!-- SECTION:DESCRIPTION:END -->
