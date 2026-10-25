---
id: TASK-187
title: >-
  Dry-run JSON: a failed in-use scan shows held_by_deleted_files: [] (same as
  none)
status: Done
assignee: []
created_date: '2026-10-25 05:34'
updated_date: '2026-10-25 12:45'
labels:
  - shipping
  - json
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-166 walk: in a dry run where no candidate reaches the in-use step, a failed scan appears only on stderr and the JSON says held_by_deleted_files: [], indistinguishable from 'none held'. JSON contract change: needs a decision (e.g. null when unknown).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decision 2026-10-25 by mark-emulator (Mark delegated; Mark may veto): null when unknown, never []. A failed scan gives held_by_deleted_files: null in live and dry runs.

2026-10-25 inuse2 ee643a6 on branch inuse2 (not merged; base master 28e9456). See commit message for the change. Report.held_by_deleted_files: Option; null when the scan failed/stopped/did not settle or none was made (incl. goal already met: Mark may veto that widening), reset per tier. reference.md Output updated (and estimated_bytes 'null when unknown'). Evidence: a_failed_scan_reports_held_by_deleted_files_as_null (live and dry; master [] both). Container inuse_held_by_deleted_files_lists_open_deleted_files and agent_harness journey PASS.

Merged to master as ee643a6 on 2026-10-25T12:45Z.

Decision 2026-10-25 by mark-emulator (Mark may veto): held_by_deleted_files is null whenever diskgc did not scan (incl. goal already met, exit 0), not only when a scan failed - 'null when unknown, never []'.
<!-- SECTION:NOTES:END -->
