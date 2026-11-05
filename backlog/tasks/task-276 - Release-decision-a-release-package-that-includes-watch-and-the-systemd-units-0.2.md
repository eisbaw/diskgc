---
id: TASK-276
title: >-
  Release decision: a release package that includes watch and the systemd units
  (0.2)
status: To Do
assignee: []
created_date: '2026-10-31T04:49:00Z'
updated_date: '2026-11-05T21:04:00Z'
labels:
  - release
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-44 review (mped S3): the development package's reclaiming watch exits 2 at the pre-release sandbox gate outside test containers (correct, by design), so a laptop owner can only run watch --tally-only until a release package ships watch. The units ship in the dev package only (0.1 is the ensure MVP, TASK-67).

Mark decides here (the first boxes of the 0.2 release criteria):
1. Scope: is 0.2 the release descriptor's provisional 0.2 row (crates/diskgc-core/src/releases.json), with or without a reclaiming watch (the release catalog decision for watch), with which catalog, and does the NixOS module (TASK-45) ship?
2. 0.1.0 exposure: a 0.1.x for the safety fixes that apply to 0.1.0 (TASK-242, 260 and later; the draft notes' Changes since 0.1.0 say which), or 0.2 supersedes 0.1.0 with a notice to its users.

Everything else a 0.2 release needs (RC QA: e2e-full, release runners, docs-vs-binary on the RC artifact; release notes with limits; release-verify) is the checked list in docs/architecture.md, Release procedure, '0.2 release criteria' (TASK-315), its single home. Needs Mark.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-31 correction: the TASK-260 backport patch is no longer at tmp/reviews/ (tmp/ cleaned); it is preserved as branch wip/t260-backport-0.1 (f8efc61 on top of release-0.1; untested). 0.1.x safety list so far: TASK-242 (container storage), TASK-260 (protect/bind), TASK-44's two --allow-root gaps, TASK-277 (non-root without provisioning).

2026-11-01 (TASK-280 review): a branch build with a plain version prints the same --version as the tag; before any build of wip/t260-backport-0.1 counts as evidence, bump it to 0.1.1-dev (release procedure in architecture.md, TASK-280). Until TASK-276 decides, the gate-lifted master build refuses a reclaiming watch (fail closed; only --tally-only), so a host watch dogfood needs Mark's go and that refusal lifted.

2026-11-01 (TASK-289): the release descriptor's 0.2 row (releases.json) is PROVISIONAL: commands ensure, watch --tally-only, check, profiles; catalog label 0.1 (same three sources); ships both watch unit templates; the draft notes document tally-only instances. A reclaiming watch stays refused; flipping it is a one-row code change plus the prose that names TASK-276. Mark decides: is 0.2 = this row, with or without reclaiming watch? Also: the draft 'Changes since 0.1.0' now lists per fix whether 0.1.0 is exposed (static reading of v0.1.0): nearly every safety fix since 0.1.0 applies to it (protect/negation cache fixes, container storage (none in 0.1.0), bind places, incomplete process list, loop devices) - input to the 0.1.x vs supersede decision.

2026-11-02 (TASK-315): the 0.2 release criteria are one checked list in docs/architecture.md (Release procedure, '0.2 release criteria'); this task's two decisions (scope; 0.1.0 exposure) are its first boxes. The PRD's Scope now names 0.2 as open, pending this decision.

Mark 2026-11-05 (answers): 0.2 may run the package managers' own clean subcommands for the lock-safe five only: pip-cache, ccache, jest-cache, uv-cache, pnpm-store. npm, go-build, yarn, platformio wait (no lock vs their own cleaner).

Mark 2026-11-05 (answers): 0.2 ships a RECLAIMING watch (not only --tally-only); 0.2 supersedes 0.1.0 (no 0.1.x): 0.2's release notes carry a notice listing which 0.1.0 flaws it fixes and tell 0.1.0 users to upgrade.
<!-- SECTION:NOTES:END -->
