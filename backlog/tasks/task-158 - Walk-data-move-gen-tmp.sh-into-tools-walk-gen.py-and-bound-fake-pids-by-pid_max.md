---
id: TASK-158
title: >-
  Walk data: move gen-tmp.sh into tools/walk/gen.py and bound fake pids by
  pid_max
status: To Do
assignee: []
created_date: '2026-10-24 23:17'
labels:
  - walk
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-65: the release-catalog walk's /tmp generator (tmp/walks/2026-10-24-release-catalog/gen-tmp.sh) generated pids above the host's pid_max, which made 803 candidates 'filtered'. Fold it into the repo's walk generator with pids bounded by /proc/sys/kernel/pid_max.
<!-- SECTION:DESCRIPTION:END -->
