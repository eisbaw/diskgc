---
id: TASK-25
title: >-
  Differential contract tests: materialize models and compare ModelHost with
  LinuxHost
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-17 08:25'
labels:
  - wave-2
  - testing
dependencies:
  - TASK-24
references:
  - docs/testing.md#4-differential-contract-tests
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the model is only useful if it behaves like the kernel. Implement materialize(model) on the scratch tmpfs (files, owners via subuid, bind/stacked mounts, symlinks, helper processes holding locks/files) and run the same scenarios on both hosts; mark model-only fields.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Model-only fields are rejected by materialize with a clear message
- [ ] #2 Outcomes and the deletion-oracle diff match between ModelHost and LinuxHost for all fixtures and 100 generated models per run (call traces are not compared: the Host trait is tentative)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-14: model-only fields and behaviours to reject or skip in materialize: statx_missing, kernel, clock/elapsed and advance events, size/other_used exact statvfs, proc: denied|failed, ext4/nfs/btrfs fs types. ModelHost answers an empty fd table/maps/environ as Ok(empty) where LinuxHost reports Failed, and its link text is the open-time path; compare outcomes, not texts.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
