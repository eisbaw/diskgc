---
id: TASK-317
title: >-
  Load flakes in the gate: overlay copy-up unit test's 10 s namespace helper;
  release suites' 300 s budget; dev shell sets TMPDIR but not TMP/TEMP/TEMPDIR
status: To Do
assignee: []
created_date: '2026-11-02T13:27:00Z'
updated_date: '2026-11-02T14:17:00Z'
labels:
  - test
  - flaky
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-311's three just e2e runs (2026-11-02, host load 16-27 from another project): (1) diskgc-linux host::tests::a_lock_file_on_an_overlay_is_not_copied_up timed out its 10 s namespace helper under I/O pressure (log tmp/worktrees/t311/tmp/t311-logs/e2e-final-run1-flake.log, copied to tmp/evidence-t311/); (2) the release-debian suite hit its 300 s budget: 10/10 finished tests passed, the 11th was cut off; these tests run one at a time in the host PID namespace and took 58-70 s each under load (e2e-final-run2-release-budget.log). Fix the waits (event-driven, or a budget scaled to measured load) rather than raising budgets blindly; never weaken a safety guard. (3) The dev shell sets TMPDIR but not TMP/TEMP/TEMPDIR, which still point into /tmp/nix-shell.*; set them to the same scratch. Also: nix develop leaves two empty /tmp dirs per invocation (Nix 2.34.8 upstream; /tmp holds ~58k nix-shell.* dirs from every agent) - report upstream or document.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-02 recurrence (TASK-315 gate, docs-only diff on c2005b4): nix develop -c just check exit 101, only host::tests::a_lock_file_on_an_overlay_is_not_copied_up: 'wait for the namespace helper: test child ... did not exit in 10 seconds'; 212 passed in diskgc-linux lib. Host load 24.5/21.8/18.9, PSI io some avg60 17.9, concurrent rustc builds (another worktree). Raw log: tmp/evidence-t315/check-run1-flake.log (main checkout). Not resolved by the rerun.
<!-- SECTION:NOTES:END -->
