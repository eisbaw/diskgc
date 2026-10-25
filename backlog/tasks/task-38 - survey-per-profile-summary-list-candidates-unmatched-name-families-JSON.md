---
id: TASK-38
title: 'survey: per-profile summary, --list candidates, unmatched name families, JSON'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-25 20:18'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Orchestrator 2026-10-23 (design audit): now depends on TASK-116 (survey --json schema design) - do not implement against the planned/intended syntax.

Orchestrator 2026-10-24 (COMPASS round 2): dependency changed from TASK-34 to TASK-30 (survey needs the snapshot, not hardening wave 3).

COMPASS round 4 (2026-10-25): survey depends on the tally's discovery pass (29), not the snapshot (30): one way to walk profile finds.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
