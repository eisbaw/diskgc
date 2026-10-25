---
id: TASK-217
title: >-
  ensure --min-free above the filesystem size: message repeats bytes and
  rounding reads '1.5 GiB is more than 1.5 GiB'
status: To Do
assignee: []
created_date: '2026-10-25 12:10'
labels:
  - shipping
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk Debian walk: ensure --min-free 1560M ~ on a 1.5 GiB tmpfs prints: --min-free 1635778560 bytes: 1.5 GiB (1635778560 bytes) is more than the filesystem's size (1.5 GiB; 1610612736 bytes). Echo what the user typed (--min-free 1560M) and give enough precision (or MiB) that the comparison reads true. Same text in the JSON error message. Dumps: tmp/walks/2026-10-25-rc-walk-debian/dumps/05-dry-config.txt, tmp/walks/2026-10-25-rc-walk-debian/dumps/11-json-exhausted.txt.
<!-- SECTION:DESCRIPTION:END -->
