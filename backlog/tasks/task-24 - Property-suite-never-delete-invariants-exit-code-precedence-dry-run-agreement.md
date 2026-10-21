---
id: TASK-24
title: >-
  Property suite: never-delete invariants, exit-code precedence, dry-run
  agreement
status: Done
assignee:
  - '@codex'
created_date: '2026-10-16 22:41'
updated_date: '2026-10-21 22:43'
labels:
  - wave-2
  - testing
dependencies:
  - TASK-23
references:
  - docs/testing.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Prove 0.1 filesystem deletion safety on generated ModelHost inputs: call-trace and owner invariants, exit-code precedence, supported filesystem selection set laws, and stable ordered dry-run agreement. Save and replay a shrunk YAML model for a planted unchecked unlink trace. Full random profile/expression selection remains TASK-16; index authority and indexed adaptive differences remain TASK-108.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The property suite runs in just check with few cases and in just soak with many
- [x] #2 A planted bug (deleting one unchecked entry) is caught and shrunk to a small regression file
- [x] #3 The 0.1-relevant properties in docs/testing.md section 3 have generated ModelHost checks: independent model-spec deletion trace, owner, symlink and mount safety; generated Cargo lock preservation; exit-code precedence; supported filesystem selection set laws; and ordered dry-run agreement on stable non-overlapping candidates. TASK-16 owns the full random profile/expression reference and TASK-108 owns index and adaptive ordering properties
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: 'just soak' is a placeholder (prints NOT RUN, exits 0). Replace it when the property suite exists.

Scope split 2026-10-21: PRD 0.1 excludes the index. TASK-108, dependent on TASK-28/29, owns stale/partial/corrupt index authority and indexed best-fit/overlap dry-run differences. TASK-16 already owns full randomized profile/expression selection reference. This task covers the 0.1 filesystem deletion and selection subset; the original blanket AC3 could not be completed honestly before those dependencies.

2026-10-22 implementation evidence: nix develop -c just check passed workspace fmt, Clippy and all tests, including 5 new property tests at 32 cases/property. nix develop -c just soak passed all 5 with 1,024 cases/property in 58.38 s. just --list and just --summary parse. The planted mutation is an extra successful Unlinkat call inserted into the observed ModelHost trace after a real model deletion; the shared checked-entry oracle rejects it, proptest shrinks optional candidates to mask 0, and regressions/unchecked-entry.yaml is parsed and replayed. This proves trace-oracle sensitivity; it is not a production source-code mutation or a real extra delete. The 0.1 dry-run property covers stable non-overlapping candidates in oldest order without index; overlap/best-fit is TASK-108.

2026-10-22 review fix supersedes the earlier planted-trace limitation and 5/5 count: the test now creates /tmp/unchecked in ModelHost, performs an actual statx then unlinkat outside the candidate after normal deletion, and the shared checked-entry oracle rejects that real model operation. Proptest shrinks optional candidates to zero; the saved YAML fixture replays it. The generated filesystem property independently enumerates paths, owners and symlinks from HostModel YAML and refuses nested mounts from its mount list, then checks trace paths and actual unlinks against that specification. A new generated Cargo reclaim property varies debug/release, extra output entries and byte sizes, requiring every Cargo lock to retain its path and inode and no lock unlink or command operation. Final focused 6/6; just soak 6/6 in 108.83 s (five generated properties x 1,024 cases, plus separate planted-regression proof); just check passed fmt, Clippy, all workspace tests and doctests. Parent still needs E2E and QA/MPED re-review before task completion.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Five generated ModelHost properties run at 32 cases in just check and 1024 in just soak; the separate planted unchecked ModelHost unlink shrinks to and replays a saved YAML regression. Independent model-spec oracle covers fs paths/owners, symlink and mount refusal; generated Cargo reclaim preserves lock paths/inodes. Index/adaptive proof is TASK-108 and full random selection reference remains TASK-16. Docker PID-ready publication race found by full gate was fixed with same-directory atomic rename. Final nix develop -c just e2e passed: 843 workspace tests, 1 intentionally ignored; 169/169 Nix containers and 7/7 Docker. QA and MPED GO. No host deletion as part of testing. Coverage limit: one generated Cargo lock layout and no generated checkout-marker names.
<!-- SECTION:FINAL_SUMMARY:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
