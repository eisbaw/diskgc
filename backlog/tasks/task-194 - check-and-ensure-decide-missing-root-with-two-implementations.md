---
id: TASK-194
title: check and ensure decide 'missing root' with two implementations
status: To Do
assignee: []
created_date: '2026-10-25 05:59'
labels:
  - shipping
  - structure
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the 10-task codebase review at 72b376c (2026-10-25). main.rs:warn_missing_roots (raw openat2 via path_is_absent) vs invocation.rs:refuse_missing_roots (resolve_in over Host). TASK-178 had to be fixed twice; model vectors cannot cover check; reference.md ### check still says 'a stat of each supplied root'; roots failing the mount screen are silently not warned. Fix: make refuse_missing_roots generic over Host, call it from check, delete path_is_absent, update doc. Also widen TASK-187: executor.rs:held reports [] when the scan fails in a live run too.
<!-- SECTION:DESCRIPTION:END -->
