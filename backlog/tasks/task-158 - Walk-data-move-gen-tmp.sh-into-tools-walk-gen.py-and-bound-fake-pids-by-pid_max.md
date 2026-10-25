---
id: TASK-158
title: >-
  Walk data: move gen-tmp.sh into tools/walk/gen.py and bound fake pids by
  pid_max
status: Done
assignee: []
created_date: '2026-10-24 23:17'
updated_date: '2026-10-25 03:52'
labels:
  - walk
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-65: the release-catalog walk's /tmp generator (tmp/walks/2026-10-24-release-catalog/gen-tmp.sh) generated pids above the host's pid_max, which made 803 candidates 'filtered'. Fold it into the repo's walk generator with pids bounded by /proc/sys/kernel/pid_max.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented on branch walk-tools 4e707b8 (unmerged). Evidence tmp/walks/2026-10-25-walk-tools/. Gate: nix develop -c just check exit 0 (1116 passed, 0 failed, 1 ignored; unittest 34 OK). just e2e not run. gen.py --tmp-bulk N (<=8000, free-space checked) folds gen-tmp.sh's bulk shapes; owner pids in [pid_max/2, pid_max), live ones skipped; bulk 0 tree name-identical to master. In wt-c (WALK_PID_HOST=1, bulk 4000) a dry run classified 4162 candidates, 0 filtered (13-*). Real producers of gen-tmp.sh (real nix-shell/nix develop/pytest, SIGKILLed ones) not folded.

Merged to master as aaa6c36 on 2026-10-25T03:52Z.
<!-- SECTION:NOTES:END -->
