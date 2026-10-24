---
id: TASK-127
title: 'Documented skip reasons with no producer: unsupported_store, has_not, stale'
status: To Do
assignee: []
created_date: '2026-10-23 21:59'
updated_date: '2026-10-24 08:32'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From TASK-121 (2026-10-24): every_skip_reason_has_a_vector exempts these three by name and fails when one enters Reason::ALL, is quoted as a reason in diskgc-core (docker_backend.rs, executor.rs, or a line with 'reason'), or shows in a vector. Review suggestion: make the report's skip reason a closed enum (Reason plus the backend reasons; JSON unchanged), so 'has a producer' becomes 'is in ALL' and the source scan can go.

Orchestrator 2026-10-24 (COMPASS round 2): 'stale' gets its producer in TASK-31, 'unsupported_store' is settled in TASK-118/36; 'has_not' should be removed from the enum. All must be settled before any release freezes the JSON (0.1 included).

From TASK-118 (2026-10-24, branch task118-design): unsupported_store REMOVED from the documented JSON skip enum (store selection is part of backend binding: unsupported_backend or remote_backend) and from REASONS_WITHOUT_PRODUCER / report tests' backend_reasons. backend_unavailable ADDED (Mark's Docker ruling; producers TASK-36 and TASK-113), exempted in REASONS_WITHOUT_PRODUCER until a producer lands. has_not and stale unchanged here.
<!-- SECTION:NOTES:END -->
