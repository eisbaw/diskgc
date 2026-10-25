---
id: TASK-189
title: In-use scan has no deadline or interrupt check (breaks --timeout + 10 s)
status: To Do
assignee: []
created_date: '2026-10-25 05:59'
labels:
  - release-blocker
  - prd
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the 10-task codebase review at 72b376c (2026-10-25). inuse/scan.rs:gather and inuse/mod.rs:FreshAttempts::retry: up to 16 listings plus an unsettled retry can take tens of seconds with no kernel block, breaking the PRD's 'ensure returns within --timeout + 10 s'; the retry sleep ignores signals. Fix: pass the deadline and interrupted() into gather, check between listings, return Stop::DeadlinePassed.
<!-- SECTION:DESCRIPTION:END -->
