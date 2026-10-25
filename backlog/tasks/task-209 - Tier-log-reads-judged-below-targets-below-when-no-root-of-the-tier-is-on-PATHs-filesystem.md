---
id: TASK-209
title: >-
  Tier log reads 'judged below  (' / 'target(s) below ' when no root of the tier
  is on PATH's filesystem
status: Done
assignee: []
created_date: '2026-10-25 12:09'
updated_date: '2026-10-25 14:27'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the rc-walk Debian walk (release-01-candidate at rc-walk 80c858c, new-user walk). With the example config, `ensure --dry-run --min-free 1530M ~` logs
`tmp-pytest-sessions: ...: 0 candidate(s) judged below  (0 found again ...)` and on /tmp `cargo-old: ...: 0 target(s) below ` (empty root list, double space). A new user cannot tell whether the tier looked anywhere. Say why: e.g. `no root on this filesystem (/tmp is on another)`. Source: executor.rs, the two log lines joining tier.roots. Dumps: tmp/walks/2026-10-25-rc-walk-debian/dumps/06-dry-config-home.txt, tmp/walks/2026-10-25-rc-walk-debian/dumps/07-dry-config-tmp.txt.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 polish (branch polish 7abae1b (not merged)): rootless tier logs 'nothing searched: no root of the tier is on PATH's filesystem (skipped as other_fs)' (fs plan, fs reclaim, cargo, and union lines). Walk: tmp/walks/2026-10-25-polish/dumps/b06-dry-config-home.txt -> tmp/walks/2026-10-25-polish/dumps/a06-dry-config-home.txt, tmp/walks/2026-10-25-polish/dumps/a07-dry-config-tmp.txt.

Merged to master as 72fccd8 on 2026-10-25T14:27Z.
<!-- SECTION:NOTES:END -->
