---
id: TASK-82
title: >-
  ensure human summary: name what was kept and why, held deleted files, time
  taken
status: Done
assignee: []
created_date: '2026-10-19 02:13'
updated_date: '2026-10-23 00:25'
labels:
  - cli
  - ux
  - wave-2
dependencies: []
references:
  - docs/reference.md
  - diskgc-e2e/src/journey_suite.rs
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the CI operator journey (task-12, journey_ci_operator_frees_space_with_ensure). The human-mode result of a run that could not meet its goal reads, verbatim:

  exhausted: 15.4 GiB available on /tmp (min-free 15.4 GiB, target-free 15.4 GiB); 0 action(s) (observed change +0 B), 5 skipped, 0 error(s)

What an operator reading the job log cannot tell from it:
- which targets were kept and why: "5 skipped" lumps 3 targets an earlier run already emptied with a running test binary (in_use) and a build in progress (locked); the names and reasons are only in the stderr progress lines (one very long line per in_use target);
- that 4 MiB were held by a deleted file of a stale process (held_by_deleted_files is in the JSON only; neither the summary nor stderr mention it), which is often the actionable fact when ensure exits 1;
- how long the run took (no duration anywhere in human mode);
- "15.4 GiB available ... min-free 15.4 GiB" and the stderr line "15.4 GiB available on /tmp, below --min-free 15.4 GiB" look contradictory: both round to one decimal; the same happens on a real runner just under a 50G goal ("50.0 GiB ... below 50.0 GiB");
- "exhausted" is the JSON enum, not a sentence an operator recognises ("nothing more could be reclaimed").
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The summary of a run that did not meet --min-free lists each kept candidate with its reason (aggregated per reason beyond a small number), separately from targets an earlier run already emptied
- [x] #2 held_by_deleted_files is summarised (count, bytes, top pids) in human mode when non-empty
- [x] #3 The run's duration is printed
- [x] #4 Two compared sizes that round to the same text are printed with enough precision to differ
- [x] #5 Covered by report unit tests and asserted in the CI operator journey
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Order (task-12 round 2 review): do AC #4 (the rounding contradiction "15.4 GiB available, below --min-free 15.4 GiB") first, together with task-86's "reclaiming X" printed before the target is judged; both make the log say something false.

2026-10-23: Integrated human ensure summary change on main after TASK-16; focused real-binary CI operator journey passed 1/1. Full exact just e2e, parallel QA/MPED reviews, and commit still pending. No host deletion.

2026-10-23: Exact nix develop -c just e2e passed on final code (workspace, Nix fast, Docker, five VMs). Focused CI operator journey passed 1/1; QA and MPED final GO. Human dry-run and normal summaries now cover elapsed time and held deleted files; preflight skips are separate from named kept candidates. No host deletion.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
