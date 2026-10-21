---
id: TASK-106
title: Per-profile YAML checkout guard override for built-ins
status: Done
assignee:
  - codex
created_date: '2026-10-21 19:54'
updated_date: '2026-10-21 21:37'
labels:
  - configuration
  - safety
  - profiles
dependencies:
  - TASK-80
references:
  - docs/reference.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Operators need a narrow YAML policy setting that can disable the VCS checkout guard for a named profile while keeping its built-in matcher and action intact. The guard remains enabled unless the effective profile explicitly allows checkouts. This includes npm-cache and uv-cache once those profiles are admitted by the applicable release catalog; an override cannot expand that catalog.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A YAML policy can set checkouts: allow or refuse by profile name without duplicating a built-in profile definition; effective policy is visible in the plan and documented. Omitted setting preserves the built-in default and the global safety default is refuse.
- [x] #2 Only the checkout guard changes. The profile matcher, root constraints, delete action, age and activity checks, protection markers, ownership, and release catalog restrictions remain effective. Unknown names and conflicting policy entries fail at load time.
- [x] #3 Disposable-container integration tests cover refused and allowed checkout markers for npm-cache and uv-cache when admitted, plus excluded profiles and other safety gates. No deletion test runs on the developer host.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Add top-level profile_policies list with named checkouts allow/refuse entries; validate names, duplicate/conflicting entries, source role, and full-override conflicts at config load; apply only to effective checkout field. Prove RED/GREEN with config and release catalog tests, then exercise marker behavior in disposable-container e2e; document syntax and residual risk.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implementation lesson: checkout policy is a narrow top-level profile_policies list, applied after built-in load. Preserve cargo-target built-in allow when absent. Proposed 0.1 policy must reject excluded names even though the development library contains them. The feature-built proposed-catalog E2E binary remains sandbox-gated and does not prove the TASK-67 release artifact. The first full gate found Clippy filter_map_bool_then; fixed at source before the final gate.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Implemented per-built-in YAML checkout policy without replacing matchers or actions. Config rejects unknown, filter, duplicate, invalid and conflicting policy entries; effective policy appears in plan and profiles show. Container tests cover npm/uv default refusal, explicit allow, keep marker and proposed 0.1 catalog exclusion. RED test rejected unknown profile_policies before implementation; focused tests GREEN. Final nix develop -c just e2e exited 0: 169/169 Nix container and 7/7 Docker tests; fmt, Clippy and workspace tests passed. QA and MPED independent reviews GO. No host deletion.
<!-- SECTION:FINAL_SUMMARY:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
