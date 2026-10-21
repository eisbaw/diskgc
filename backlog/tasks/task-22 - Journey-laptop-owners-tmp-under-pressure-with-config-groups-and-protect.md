---
id: TASK-22
title: 'Journey: laptop owner''s /tmp under pressure with config groups and protect'
status: Done
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-21 12:01'
labels:
  - wave-2
  - journey
dependencies:
  - TASK-21
references:
  - PRD.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: exercise profiles, tiers and protection as a user. In a container whose /tmp mirrors the survey (pytest-of-* inside nix-shell dirs, rc dirs, a Claude Code temp dir, a live shell's TMPDIR, sockets), write a config with stale and pressure groups and protect: [claude-code-tmp], then run ensure --dry-run and ensure.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Protected and in-use entries survive; the deletion oracle agrees with the dry-run plan for independent candidates
- [x] #2 Findings are filed as tasks
- [x] #3 The fixtures use user profiles (the built-in library ships in wave 4)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-21 laptop /tmp journey test candidate integrated: disposable-container fixture uses user profiles, nested pytest, nix-shell rc/TMPDIR, Claude temp, socket, stale/pressure groups and protect; compares dry-run, real-run actions and deletion oracle. New test compiles in fresh isolated-target E2E crate and fmt/diff checks pass; it has not run under no-deletion instruction, so assumptions about protected dry-run reason, exhausted goal and tier order remain unverified. Exported TMPDIR after exec with no open fd remains undetectable; fixture keeps descriptor and initial TMPDIR. AC/DoD remain open.

2026-10-21 verification: the laptop /tmp journey passed in the full disposable Nix-container E2E suite (152/152). Dry-run and real-run eligibility agreed, protected and in-use data survived, and the deletion oracle was clean. QA and MPED reviewed the staged patch; AC2 remains open until journey findings are reconciled in the tracker.

2026-10-21 journey reconciliation: the container laptop_tmp_groups_protect_live_work_and_reclaim_independent_cruft passed the settled full nix develop -c just e2e gate (167/167 Nix image, 7/7 native Docker), with clean deletion oracle. The no-open-descriptor live TMPDIR/pytest-session blind spot found by this journey is recorded and reproduced under TASK-65 (and throughput follow-up TASK-70); those unsafe broad built-ins remain unshipped. AC2 is satisfied by these existing tracker tasks; TASK21 dependency is Done. Earlier QA/MPED review of the journey found no blocker, and docs/testing.md DoD is met for this journey.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
