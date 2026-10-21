---
id: TASK-85
title: >-
  JSON report for CI dashboards: duration, start time, version, skip evidence
  and sizes
status: In Progress
assignee: []
created_date: '2026-10-19 02:14'
updated_date: '2026-10-21 21:16'
labels:
  - cli
  - ux
  - wave-2
dependencies: []
references:
  - docs/reference.md
  - docs/ci-pre-job.sh
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the CI operator journey (task-12), reading the --json report as a fleet dashboard would. Input for the JSON contract freeze (task-26): every field added later is a contract change. What a dashboard needs and the report does not carry:
- how long the run took and when it started (only per-action duration_ms; the example hook measures itself with date +%s, 1 s resolution, and showed "after 0s");
- which diskgc produced it (no version field; a fleet runs several versions during a rollout);
- why a target was kept beyond the enum: `skipped` has reason `in_use` or `locked` only; the process (pid, command, the evidence) is only in the stderr progress line;
- how much space the kept targets hold (no size on `skipped`; `estimated_bytes` exists in `plan` only), so "could have freed X if the build had finished" cannot be shown;
- the age (last build) of what was reclaimed, to tune the idle filters (stderr logs it, as raw epoch seconds).

Decide at the freeze which of these belong in the contract (a field each, null when unknown), not in this task's code alone.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Before TASK-26 begins, each listed report item has a documented proposed contract decision: add a field to docs/reference.md Output with type, null rule and a biting test, or reject it with the reason recorded here. TASK-26 reviewers accept or revise those decisions and freeze the final JSON contract.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
task-26 depends on this task: each item is decided at the freeze.

2026-10-21 provisional contract implementation: JSON emits started_at (UTC RFC 3339 string or null before executor/unrepresentable wall clock), duration_ms (u64 or null), version (always string), skipped.detail (advisory string or null), skipped.estimated_bytes (complete prior tally or null), and actions.idle_age_seconds (prior tally change/build age sampled before action, or null). docs/reference and focused report/model/CLI tests cover types, null rules, grouping, and a clock advance during reclaim. Full just e2e passed 167/167 Nix and 7/7 native Docker. Structured process blocker remains undecided: generic in_use may lack a unique process and flock cannot identify its holder; do not parse detail text into PID. AC/DoD remain open until TASK26 freeze and explicit decision.

2026-10-21 read-only contract design: propose skipped[].process_blockers: null or a nonempty array of {pid: u32, command: string|null, evidence: stable enum string}. PID and command are scan-time observations. null means no reliably attributable process, including flock contention, unsettled scan/churn, generic and grouped skips. Preserve detail as human diagnostic; never parse it for structured facts or invent PID 0. Carry typed evidence through Skip and Cargo evidence paths; cover fd/environment/unreadable, multiple Cargo blockers, null cases and grouped rows in tests. This is a proposal, not a freeze decision or verified implementation.

2026-10-21 structured process blocker candidate implemented: skipped[].process_blockers is null or a nonempty array of {pid, command|null, evidence stable enum}. Typed evidence originates in the process scan and Cargo checks, not parsed detail; scan-time PID/name observations are advisory. Flock, unsettled scan, generic and grouped skips remain null. Executor-to-JSON tests assert unnamed unreadable PID command:null and two Cargo holders, plus focused evidence cases. Initial full gate passed 167/167 Nix and 7/7 Docker; after review followups, final stable-tree nix develop -c just e2e exited 0 with 167/167 Nix, 7/7 Docker, workspace fmt/clippy/tests and host interlock exit 86. Parallel QA and MPED reviews GO for incremental commit. TASK85 AC/DoD stay open until TASK26 freeze decides the field and whole JSON contract.

Compass dependency review, 2026-10-21: TASK-26 depends on TASK-85, so TASK-85 must finish its proposed decisions before the freeze. The final decision remains with TASK-26; this clarifies sequencing without claiming the current proposal is frozen.

This sequencing note supersedes the earlier statement that TASK-85 itself waits for the freeze: proposed decisions complete TASK-85; final accept/revise decisions complete TASK-26.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
