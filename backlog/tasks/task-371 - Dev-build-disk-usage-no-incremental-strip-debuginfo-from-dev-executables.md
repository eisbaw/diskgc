---
id: TASK-371
title: 'Dev build disk usage: no incremental, strip debuginfo from dev executables'
status: Done
assignee: []
created_date: '2026-11-07T20:14:00Z'
updated_date: '2026-11-09T09:43:00Z'
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

ee45a120 on t371-dev-disk (unmerged): [profile.dev] debug=false, strip=debuginfo, incremental=false in root, diskgc-e2e and fuzz Cargo.toml. Cold just check tmp/target 5.95 -> 1.89 GiB. Panic/proptest file:line kept; backtrace frames lose file:line. Cost: edit-rebuild 90 s vs 18 s with CARGO_INCREMENTAL=1 (flag to Mark). just check rc=0, just e2e rc=0.

Merged ee45a12040f8209886044a1298f0b197dd8d8743 (2026-11-09). Trade-off for Mark: edit-rebuild ~5x slower without incremental (90 s vs 18 s); alternative: incremental on by default, CARGO_INCREMENTAL=0 only in gate recipes.
<!-- SECTION:NOTES:END -->
