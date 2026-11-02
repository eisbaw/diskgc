---
id: TASK-315
title: >-
  Write the 0.2 release criteria as a checked list; PRD refresh (VM non-goal,
  real-filesystem risk, TASK-169 pending marker)
status: In Progress
assignee: []
created_date: '2026-11-02T11:33:00Z'
updated_date: '2026-11-02T14:01:00Z'
labels:
  - release
  - docs
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-02: 'ensure we tackle these findings ... we need more real-world VM tests, so we need to bake into the e2e tests that we do clean up afterwards.' From COMPASS consult #3 (master c286b3b). COMPASS: there is no written 0.2 release-criteria list (only prose in TASK-276). Write it (docs/reference.md release notes draft or architecture.md Release procedure, single home): each criterion with the evidence command. Refresh the PRD where stale: the non-goal 'a VM test suite, for now', the risk 'No real block filesystems ... in e2e', the TASK-169 'pending Mark's confirmation' marker (TASK-169 is Done; point to TASK-296/305); docs/testing.md 'Not covered end-to-end' still lists btrfs. PRD scope/safety changes stay Mark's: propose wording, mark it pending.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-02: in progress in tmp/worktrees/t315 (branch t315-release-criteria).
<!-- SECTION:NOTES:END -->
