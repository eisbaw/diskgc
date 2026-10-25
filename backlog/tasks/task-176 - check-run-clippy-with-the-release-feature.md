---
id: TASK-176
title: 'check: run clippy with the release feature'
status: Done
assignee: []
created_date: '2026-10-25 02:42'
updated_date: '2026-10-25 09:55'
labels:
  - followup
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-148 merge: just check now runs diskgc's release-feature tests but not clippy with --features proposed-release-01-catalog, so lints in feature-gated code go unseen.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
lane-c (2505e69): just check runs cargo clippy --locked -p diskgc --all-targets --features proposed-release-01-catalog -- --deny warnings before the feature test. Green in just check on 6c1e6ec. Awaiting merge.

Merged to master as 9f317cb on 2026-10-25T09:54Z.
<!-- SECTION:NOTES:END -->
