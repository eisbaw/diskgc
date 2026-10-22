---
id: TASK-111
title: Make filesystem union retention guards veto shared candidates across terms
status: Done
assignee: []
created_date: '2026-10-22 13:05'
updated_date: '2026-10-22 15:38'
labels:
  - safety
  - selection
  - irreversible
dependencies:
  - TASK-15
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A filesystem union currently admits a candidate when any positive term passes its per-term Judge. A keep_newest or link/current/installed protection guard on another term can return Protected, yet plan_fs_union and walk_fs_union retain the passing term and may delete that same inode. Retention guards should veto deletion of a shared candidate regardless of another term. Resolve and document guard scope across overlapping source terms, while preserving term-local evidence filters and source action semantics.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A candidate matched by two filesystem union terms is retained when any matching source or positive filter marks it keep_newest, protected by a link/current link, or installed by a manifest, regardless of union order.
- [x] #2 Dry-run and reclaim use the same guard decision for shared candidates; a disposable-container real-binary fixture proves the protected candidate survives and an unprotected sibling is reclaimed.
- [x] #3 The documented union semantics distinguish global retention guards from term-local evidence filters, and property/reference tests cover commutativity and guard veto.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
This is a safety prerequisite of TASK-16. Its initial dependency on TASK-16 was reversed after review; TASK-16 remains open until the shared-candidate guard bypass is fixed.

2026-10-22 implementation checkpoint: isolated worktree draft records raw guarded structural matches before source-owned evidence, keeps guard-only contributors separate from action-admitted sources, and vetoes shared inode deletion in plan, prequeue, locked delete and command paths, with final pre-action recheck. MPED found and the worker fixed a source-owned where false/unknown bypass; Compass withdrew its initial GO and confirmed raw source coverage semantics. Focused model 82/82 passed after two review-driven corrections; the 40k stress case passed in the earlier full run. Disposable real-binary Podman delete/command union case passed 1/1 in both orders, retaining guarded current and reclaiming obsolete sibling. Integrated exact e2e and final parallel QA/MPED review remain before any acceptance criterion or Done is checked.

2026-10-22 full-gate interruption: exact just e2e first run exited 101 in an unrelated LinuxHost in-use test after core 220/220. A bounded logged rerun reproduced the failure on repetition 4: the process scan was unsettled after 16 listings and correctly returned generic in_use, while the host test incorrectly inferred settled attribution from all unreadable processes being excused. The captured failing log is in ignored repo tmp/task111-linux-loop.9eZmcx/run-04.log. Corrected the test to branch on Scan::blocked(): generic refusal is asserted for unsettled scans; holder PID and clear sibling remain required for settled scans. Final repeated-suite proof and exact full gate remain pending.

2026-10-22 final gate: exact nix develop -c just e2e exited 0 on integrated tree after the captured LinuxHost test correction: fmt, Python 4/4, Clippy deny-warnings, workspace tests (core 220/220, Linux 141 pass + 1 ignored, fs_reclaim 83/83), Nix harness 101/101, live Nix container 219/219, Docker 7/7, and all five VM checks passed. The new real-binary union guard case passed. Twenty full Linux lib suite repetitions passed; run 10 exercised an unsettled scan and validated the corrected conservative test branch. Parallel QA and MPED final review GO; git diff --check clean. No host data deleted. Candidate-wide guards, both action paths, reference/property tests and documentation satisfy AC1-3.
<!-- SECTION:NOTES:END -->
