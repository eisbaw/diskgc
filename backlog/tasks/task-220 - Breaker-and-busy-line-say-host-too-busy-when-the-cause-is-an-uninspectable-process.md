---
id: TASK-220
title: >-
  Breaker and busy line say 'host too busy' when the cause is an uninspectable
  process
status: To Do
assignee: []
created_date: '2026-10-25 12:44'
labels:
  - shipping
  - ux
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From inuse2 review (TASK-190 follow-up). Since TASK-190 an unexcused unreadable process (e.g. a newgrp shell) makes candidates in_use_unknown and trips the 3-in-a-row breaker. The per-candidate human line names the process, but the breaker's unchecked skips ('not checked: the host was too busy ...', executor too_busy_to_check) and report busy_line ('run again when the host is less busy') still blame load; rerunning will not help. Carry the first blocker (pid, command) into the breaker's skip detail and phrase the advice by cause.
<!-- SECTION:DESCRIPTION:END -->
