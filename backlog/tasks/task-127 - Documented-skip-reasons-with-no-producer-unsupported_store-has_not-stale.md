---
id: TASK-127
title: 'Documented skip reasons with no producer: unsupported_store, has_not, stale'
status: To Do
assignee: []
created_date: '2026-10-23 21:59'
labels:
  - contract
  - verification
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
docs/reference.md's skipped[].reason enum lists unsupported_store, has_not and stale, but no code path produces them (grep crates/diskgc-core: they appear only in report tests' backend_reasons list). has_not is a predicate name, not a skip reason in the code (a failed has_not is 'filtered' or no candidate); stale is an index reason (docs/architecture.md) for an index that does not exist yet; unsupported_store has no producer (Nix kind or Docker containerd layouts?). Found by TASK-121's every_skip_reason_has_a_vector, which exempts them by name. Decide per reason: remove from the frozen JSON enum before 0.1, or implement its producer with a vector; then drop the exemption in crates/diskgc-model/tests/profile_vectors.rs (REASONS_WITHOUT_VECTOR).
<!-- SECTION:DESCRIPTION:END -->
