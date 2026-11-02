---
id: TASK-305
title: >-
  Decide: TASK-299 loop-alias limits (orchestrator convergence call) vs the PRD;
  cost the upper-layer reverse-alias fail-closed rule
status: In Progress
assignee: []
created_date: '2026-11-01T20:29:00Z'
updated_date: '2026-11-02T18:06:00Z'
labels:
  - needs-mark
  - safety
  - irreversible
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
COMPASS 2026-11-01: the orchestrator documented four TASK-299 aliases as limits in safety.md ('Limits of the loop rules') citing the race-convergence policy, but that policy lets only MULTI-event races become limits; these are 0-event static configurations, which by its logic block wherever a fix exists. PRD Safety boundaries say without qualification that diskgc never deletes a file that backs a configured loop device, and the PRD has no overlay limit. Per limit (COMPASS): (1) overlay only in another mount namespace - sound, falls in the other-namespaces blind-spot class (TASK-303); (2) idmapped private overlay layer clones and (3) eCryptfs over them - sound as limits (root-made, exotic); (4) reverse alias through a LOWER layer - likely harmless (deleting through the overlay writes a whiteout; lower survives), unverified; (5) reverse alias through an UPPER layer in diskgc's own namespace - real data loss on detach; NOT to be waived untested. Candidate fail-closed rule: read the overlay's upperdir from its mountinfo super options; when an exact loop identity of an owner's file lies on that filesystem, mark the overlay's candidates in_use_unknown. Steps: implementer costs rule (5) (model + red test), verifies (4); Mark rules: ratify (1)-(3) with a PRD Limits sentence, and fix or waive (5).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-02: item (5) in progress in tmp/worktrees/t305 (branch t305-overlay-upper-alias); items (1)-(3) await Mark.

2026-11-02: item (5) fixed and merged (exact match by upper-layer name, processes included; decoy cover detected by mountinfo order). Item (4) verified harmless on a real kernel. Items (1)-(3) and (4)'s wording await Mark's ruling; the task stays open for that.
<!-- SECTION:NOTES:END -->
