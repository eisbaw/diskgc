---
id: TASK-217
title: >-
  ensure --min-free above the filesystem size: message repeats bytes and
  rounding reads '1.5 GiB is more than 1.5 GiB'
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
rc-walk Debian walk: ensure --min-free 1560M ~ on a 1.5 GiB tmpfs prints: --min-free 1635778560 bytes: 1.5 GiB (1635778560 bytes) is more than the filesystem's size (1.5 GiB; 1610612736 bytes). Echo what the user typed (--min-free 1560M) and give enough precision (or MiB) that the comparison reads true. Same text in the JSON error message. Dumps: tmp/walks/2026-10-25-rc-walk-debian/dumps/05-dry-config.txt, tmp/walks/2026-10-25-rc-walk-debian/dumps/11-json-exhausted.txt.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 polish (branch polish 7abae1b (not merged)): '--min-free 1560M: 1.52 GiB (1635778560 bytes) is more than the filesystem's size, 1.50 GiB (1610612736 bytes)'; Size Display prints the largest exact unit (round-trip proptest). Walk: tmp/walks/2026-10-25-polish/dumps/b05-min-free-too-big.txt -> tmp/walks/2026-10-25-polish/dumps/a05-min-free-too-big.txt, tmp/walks/2026-10-25-polish/dumps/a11j-min-free-too-big.json.

Merged to master as 72fccd8 on 2026-10-25T14:27Z.
<!-- SECTION:NOTES:END -->
