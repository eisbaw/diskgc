---
id: TASK-211
title: '''process scan: N processes of other users not inspected'' repeats once per tier'
status: Done
assignee: []
created_date: '2026-10-25 12:09'
updated_date: '2026-10-25 14:27'
labels:
  - shipping
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk Debian walk: with the example config (3 tiers) stderr carries the same line three times per run. Say it once per run (or once per distinct count). Dumps: tmp/walks/2026-10-25-rc-walk-debian/dumps/06-dry-config-home.txt, tmp/walks/2026-10-25-rc-walk-debian/dumps/07-dry-config-tmp.txt.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 polish (branch polish 7abae1b (not merged)): grouped runs log each process-scan note once while its text is unchanged. Walk: tmp/walks/2026-10-25-polish/dumps/b06-dry-config-home.txt (3x) -> tmp/walks/2026-10-25-polish/dumps/a06-dry-config-home.txt (1x).

Merged to master as 72fccd8 on 2026-10-25T14:27Z.
<!-- SECTION:NOTES:END -->
