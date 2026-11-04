---
id: TASK-38
title: 'survey: per-profile summary, --list candidates, unmatched name families, JSON'
status: In Progress
assignee: []
created_date: 2026-10-16 22:41
updated_date: '2026-11-04T21:32:00Z'
labels:
  - wave-4
  - cli
dependencies:
  - TASK-29
  - TASK-116
references:
  - docs/reference.md#survey
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: categorising cruft and reviewing AI-drafted profiles both need it. Implement survey per docs/reference.md#survey, using a fresh snapshot when present.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 On the /tmp fixture, survey reproduces the manual categorisation (pytest-of-* inside nix-shell dirs, rc dirs, claude temp) and lists unmatched families
- [ ] #2 --list truncates at 1,000 per profile and reports the remainder
- [ ] #3 survey never deletes and takes no lock (oracle + lock probe)
- [ ] #4 survey works outside a container (the sandbox gate only guards deletion)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Orchestrator 2026-10-23 (design audit): now depends on TASK-116 (survey --json schema design) - do not implement against the planned/intended syntax.

Orchestrator 2026-10-24 (COMPASS round 2): dependency changed from TASK-34 to TASK-30 (survey needs the snapshot, not hardening wave 3).

COMPASS round 4 (2026-10-25): survey depends on the tally's discovery pass (29), not the snapshot (30): one way to walk profile finds.

2026-11-05: Mark chose C1 (survey). In progress in tmp/worktrees/t38 (branch t38-survey), with TASK-116's design.

2026-11-04 orchestrator: implementer done, branch t38-survey 2e4f65d1 (tmp/worktrees/t38), full just e2e exit 0 on that tree; Codex review running; open: Mark's ruling on in-use scan opt-in (TASK-116 note) - survey runs it always.

2026-11-04: Codex blocker (failed discovery shown as found 0) fixed aed9fc57 with bite test; just check exit 0; full just e2e refused at containers stage by the disk guard (12.99 GiB free < 17 GiB budget) - gate owed on aed9fc57. Codex non-blockers filed: TASK-338, TASK-339. read_ahead_plan intermittent seen in this run: TASK-337.

2026-11-04: --in-use opt-in implemented (5d3a02ef/1204e5dd, restacked onto t336 as e833f651..b5b39920 without c4de7f76, which goes to TASK-337). Full e2e on the stack running; Codex review of the in-use delta running.
<!-- SECTION:NOTES:END -->
