---
id: TASK-216
title: 'Dry-run summary ends with a dangling '':'' when nothing was eligible or kept'
status: Done
assignee: []
created_date: '2026-10-25 12:10'
updated_date: '2026-10-25 14:27'
labels:
  - shipping
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk walk probe (podman 5.8.7 default seccomp, release binary): `dry run: 54.3 GiB available on / ...; at least min-free already: a reclaim would take nothing (0 eligible, 0 kept) (took 1 ms):` with nothing after the colon. Drop the colon when no lines follow. Evidence: tmp/walks/2026-10-25-rc-walk-debian/evidence/default-seccomp-probe.txt.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 polish (branch polish 7abae1b (not merged)): colon only when lines follow. Walk: tmp/walks/2026-10-25-polish/dumps/b16b-dry-nothing.txt -> tmp/walks/2026-10-25-polish/dumps/a16b-dry-nothing.txt.

Merged to master as 72fccd8 on 2026-10-25T14:27Z.
<!-- SECTION:NOTES:END -->
