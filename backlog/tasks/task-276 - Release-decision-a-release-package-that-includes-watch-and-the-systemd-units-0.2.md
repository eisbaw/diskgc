---
id: TASK-276
title: >-
  Release decision: a release package that includes watch and the systemd units
  (0.2)
status: To Do
assignee: []
created_date: '2026-10-31T04:49:00Z'
updated_date: '2026-11-01T15:04:00Z'
labels:
  - release
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-44 review (mped S3): the development package's reclaiming watch exits 2 at the pre-release sandbox gate outside test containers (correct, by design), so a laptop owner can only run watch --tally-only until a release package ships watch. The units ship in the dev package only (0.1 is the ensure MVP, TASK-67). A 0.2 release needs: Mark's go on scope (watch + index + snapshot + units + NixOS module, TASK-45), the release catalog decision for watch, RC QA (e2e-all, full tier, release runners, docs-vs-binary on the RC artifact per phase3.1 step 5b), the TASK-67-style release notes with limits (TASK-263 hidepid, TASK-267/268 static-config limits if not fixed), and a 0.1.x decision for the safety fixes TASK-242/260 (patch for 260 at tmp/reviews/t260-backport-release-0.1.patch). Needs Mark.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-31 correction: the TASK-260 backport patch is no longer at tmp/reviews/ (tmp/ cleaned); it is preserved as branch wip/t260-backport-0.1 (f8efc61 on top of release-0.1; untested). 0.1.x safety list so far: TASK-242 (container storage), TASK-260 (protect/bind), TASK-44's two --allow-root gaps, TASK-277 (non-root without provisioning).

2026-11-01 (TASK-280 review): a branch build with a plain version prints the same --version as the tag; before any build of wip/t260-backport-0.1 counts as evidence, bump it to 0.1.1-dev (release procedure in architecture.md, TASK-280). Until TASK-276 decides, the gate-lifted master build refuses a reclaiming watch (fail closed; only --tally-only), so a host watch dogfood needs Mark's go and that refusal lifted.

2026-11-01 (TASK-289): the release descriptor's 0.2 row (releases.json) is PROVISIONAL: commands ensure, watch --tally-only, check, profiles; catalog label 0.1 (same three sources); ships both watch unit templates; the draft notes document tally-only instances. A reclaiming watch stays refused; flipping it is a one-row code change plus the prose that names TASK-276. Mark decides: is 0.2 = this row, with or without reclaiming watch? Also: the draft 'Changes since 0.1.0' now lists per fix whether 0.1.0 is exposed (static reading of v0.1.0): nearly every safety fix since 0.1.0 applies to it (protect/negation cache fixes, container storage (none in 0.1.0), bind places, incomplete process list, loop devices) - input to the 0.1.x vs supersede decision.
<!-- SECTION:NOTES:END -->
