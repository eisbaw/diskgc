---
id: TASK-185
title: EMFILE still swallowed in four /proc read paths (fail closed quietly)
status: To Do
assignee: []
created_date: '2026-10-25 05:34'
labels:
  - hardening
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-166 review: proc_root_lookup's /proc/<pid>/root open, the uid_map read in proc_status, diskgc's own status read (.ok()), and pid_table's pid-namespace reads turn EMFILE into a plain error and fail closed quietly. Reviewer's fix: an Exhausted variant of ProcError so every match must handle it.
<!-- SECTION:DESCRIPTION:END -->
