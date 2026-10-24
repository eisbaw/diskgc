---
id: TASK-127
title: 'Documented skip reasons with no producer: unsupported_store, has_not, stale'
status: Done
assignee: []
created_date: '2026-10-23 21:59'
updated_date: '2026-10-24 20:48'
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

Implemented 2026-10-24 on branch task130 (commit 2bd9c7c + review fixes 8d9ddc9). has_not removed from reference.md's skipped[].reason enum (a predicate name, never a skip reason). Closed enum adopted: PlanEntry.reason / SkipEntry.reason are engine::Reason; RemoteBackend and UnsupportedBackend added; docker_backend, the Docker recheck's 'changed' and ResolveError::skip_reason() produce Reason values (ResolveError::reason() renamed transcript_label(), diagnostic only). engine::RESERVED_REASONS = stale (TASK-31), backend_unavailable (TASK-36, TASK-113); report test pins Reason::ALL + RESERVED_REASONS == reference.md enum exactly; every_skip_reason_has_a_vector needs a vector per Reason and fails if a reserved reason enters ALL or is shown; the diskgc-core source scan is deleted. Reason derives no Ord (declaration order must not order output); JSON/summary ordering keys on as_str, unchanged. JSON contract change reviewed: mped-architect GO, qa-test-runner GO.

Merged to master (fast-forward) at 5e81db1 after nix develop -c just e2e exit 0 (2026-10-24T20:47Z; see git note on 5e81db1).
<!-- SECTION:NOTES:END -->
