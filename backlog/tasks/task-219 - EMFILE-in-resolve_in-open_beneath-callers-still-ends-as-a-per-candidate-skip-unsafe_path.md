---
id: TASK-219
title: >-
  EMFILE in resolve_in/open_beneath callers still ends as a per-candidate skip
  (unsafe_path)
status: To Do
assignee: []
created_date: '2026-10-25 12:44'
updated_date: '2026-10-25 23:31'
labels:
  - hardening
  - limits
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From inuse2 review (TASK-185 class). resolve_in/open_beneath return ResolveError::Host(e) for EMFILE/ENFILE; several callers map every Err to a per-candidate refusal: fs_kind/app.rs check (lock parent -> unsafe_path), fs_kind/plan.rs contract_veto (parent -> unsafe_path), likely others among ~20 resolve call sites. Out of descriptors says nothing about the candidate: should be Stop::Failed (exit 3), as the in-use scan and walk do. Fix at the class: a ResolveError::is_out_of_descriptors (or an Exhausted variant) and an audit of every resolve caller; model vectors with openat2 EMFILE per site.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-26 t182 (branch t182: 10eb58f, 11c8a8d, 792bd53; not merged; base master 0f4a2ec). Not folded into TASK-182: the screen adds no descriptor errors; the EMFILE mapping needs a per-caller audit.
<!-- SECTION:NOTES:END -->
