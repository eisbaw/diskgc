---
id: TASK-315
title: >-
  Write the 0.2 release criteria as a checked list; PRD refresh (VM non-goal,
  real-filesystem risk, TASK-169 pending marker)
status: Done
assignee: []
created_date: '2026-11-02T11:33:00Z'
updated_date: '2026-11-02T14:48:00Z'
labels:
  - release
  - docs
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-02: 'ensure we tackle these findings ... we need more real-world VM tests, so we need to bake into the e2e tests that we do clean up afterwards.' From COMPASS consult #3 (master c286b3b). COMPASS: there is no written 0.2 release-criteria list (only prose in TASK-276). Write it (docs/reference.md release notes draft or architecture.md Release procedure, single home): each criterion with the evidence command. Refresh the PRD where stale: the non-goal 'a VM test suite, for now', the risk 'No real block filesystems ... in e2e', the TASK-169 'pending Mark's confirmation' marker (TASK-169 is Done; point to TASK-296/305); docs/testing.md 'Not covered end-to-end' still lists btrfs. PRD scope/safety changes stay Mark's: propose wording, mark it pending.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-02: in progress in tmp/worktrees/t315 (branch t315-release-criteria).

2026-11-02 result (branch t315-release-criteria). Home: docs/architecture.md, Release procedure, '0.2 release criteria' (not the draft notes: reference.md ships whole and its draft section is cut into RELEASE-NOTES.md). Every box is unticked. Where each stands today (kept here, not in the list, because it rots):
- Scope, 0.1.0 exposure: open (TASK-276). The backport branch wip/t260-backport-0.1 still has version 0.1.0 (prints the tag's --version).
- TASK-296, 305, 308: open; TASK-305 item (5) in progress in tmp/worktrees/t305.
- Mutation bar and contract review: proposed, pending Mark.
- e2e-full: no verification note records an e2e-full run on master since the 0.1 RC (a11e77b, af3d503: full Nix 303/306).
- vm-real-kernel: last recorded green 5e5221f (44/44 twice, Linux 6.18.56); not in just e2e yet (TASK-312).
- Mutation: of the deletion path only engine/delete.rs and hostlock.rs swept (TASK-177).
- Fuzz 5 x 1800 s: no recorded run (TASK-27 AC2).
- Release notes: lack TASK-309, NFS-untested, the model-only list and the ruled limits; still carry candidate-only lines.
- release-verify: accepts a lightweight tag and does not check the row's tag (TASK-319, filed from the reviews).
PRD: factual refreshes (VM non-goal dropped, Build-and-test row, real-filesystem risk rewritten, pending markers repointed to TASK-296/305/308, document map); 0.2 scope stated as Mark's open decision. Reviews: mped-architect and qa-test-runner (read-only, frozen diff); their blockers fixed in the same branch.

2026-11-02: merged (ff), docs and backlog only; just check exit 0 (run 1 hit the TASK-317 flake).
<!-- SECTION:NOTES:END -->
