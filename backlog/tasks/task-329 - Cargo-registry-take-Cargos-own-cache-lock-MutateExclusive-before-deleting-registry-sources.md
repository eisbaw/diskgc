---
id: TASK-329
title: >-
  Cargo registry: take Cargo's own cache lock (MutateExclusive) before deleting
  registry sources
status: To Do
assignee: []
created_date: '2026-11-03T13:13:00Z'
labels:
  - safety
  - cargo
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-40 (merged 2026-11-03). cargo-registry-src deletes ~/.cargo/registry/src/*/<crate> without Cargo's package cache lock (CARGO_HOME/.package-cache, mode MutateExclusive in cargo's source at 6238f6fd61bf); a concurrent cargo extracting or reading could race. Take the lock (non-blocking, skip as in_use if held) like the cargo-target protocol does for build locks. Blocks admission.
<!-- SECTION:DESCRIPTION:END -->
