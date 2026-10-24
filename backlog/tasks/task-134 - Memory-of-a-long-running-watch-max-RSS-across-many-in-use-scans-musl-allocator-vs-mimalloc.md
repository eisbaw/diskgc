---
id: TASK-134
title: >-
  Memory of a long-running watch: max RSS across many in-use scans, musl
  allocator vs mimalloc
status: To Do
assignee: []
created_date: '2026-10-24 15:46'
labels:
  - performance
  - inuse
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-115 (2026-10-24). One host dry run (one candidate, ~600 processes): max RSS static master 61 MB, after the TASK-115 allocation cuts 18 MB, mimalloc build (older code, branch task115-mimalloc) 159 MB. 30 back-to-back scans in one process (examples/scan_timing --pytest-facts): 102 MB / 17 MB / 304 MB. Open: whether RSS grows over hours of watch (repeated scans, plans, ancestor index), for the shipped static allocator; mimalloc was not adopted (TASK-115: no freshness errors at load 13-18 after the allocation cuts), so measure the shipped build and keep the mimalloc figures as the comparison. Needs a read-only long run (watch --dry-run on the host, or scan_timing for N hours) with RSS sampled over time.
<!-- SECTION:DESCRIPTION:END -->
