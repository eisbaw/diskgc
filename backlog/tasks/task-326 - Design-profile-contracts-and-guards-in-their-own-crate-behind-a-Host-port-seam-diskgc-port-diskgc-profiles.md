---
id: TASK-326
title: >-
  Design: profile contracts and guards in their own crate behind a Host-port
  seam (diskgc-port + diskgc-profiles)
status: To Do
assignee: []
created_date: '2026-11-02T21:22:00Z'
updated_date: '2026-11-02T21:57:00Z'
labels:
  - design
  - architecture
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-02: 'i wonder if we cant have the profiles implemented as separate crates? for testability and mocking, and then diskgc imports them' -> 'yes, a md with SVG file for API/seam. and then use html review skill on it'. Orchestrator recommendation: one diskgc-profiles crate (module per family), the Host trait and core types it needs split into diskgc-port, a narrow contract/guard trait; design before code; pilot migration with the toolchain family after TASK-79/78/40 merge; absorbs TASK-323. Deliverable: docs/design/profile-crates.md + docs/design/profile-crates-seam.svg, reviewed by Mark in the html-comment UI.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-02: in progress in tmp/worktrees/t326 (branch t326-profile-crates-design).

2026-11-02: Mark: 'we wont do such a refactor now.' Parked. The design (docs/design/profile-crates.md + profile-crates-seam.svg, inventory as of master 697652d) is kept on branch t326-profile-crates-design (commit 0818a49), not merged. Revisit only on Mark's request; TASK-323 (project.rs split) stays separate.
<!-- SECTION:NOTES:END -->
