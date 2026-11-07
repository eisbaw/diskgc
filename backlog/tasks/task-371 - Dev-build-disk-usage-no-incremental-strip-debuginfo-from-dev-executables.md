---
id: TASK-371
title: 'Dev build disk usage: no incremental, strip debuginfo from dev executables'
status: In Progress
assignee: []
created_date: '2026-11-07T20:14:00Z'
updated_date: '2026-11-07T20:14:00Z'
labels:
  - tooling
  - disk
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-07: 'do we build with debug enabled? i think release builds, or aleast not building dep-crates with debug symbols, is a good idea to reduce disk usage.' -> 'yes do it'. Measured (t370 worktree): tmp/target ~9.3 GiB = incremental 4.4 + 70 test executables 3.4 (debug sections ~2/3 of each, mostly std's debuginfo; dev already has debug = line-tables-only and deps debug = false) + rlib/rmeta 1.4. Do: [profile.dev] incremental = false and strip = 'debuginfo' (keep panic file:line; check it), measure a clean build's tmp/target before/after, full gate.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
implementer in tmp/worktrees/t371 (branch t371-dev-disk)
<!-- SECTION:NOTES:END -->
