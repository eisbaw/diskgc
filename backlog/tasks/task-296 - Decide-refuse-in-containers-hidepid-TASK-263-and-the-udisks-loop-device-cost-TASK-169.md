---
id: TASK-296
title: >-
  Decide: refuse in containers / hidepid (TASK-263) and the udisks loop-device
  cost (TASK-169)
status: To Do
assignee: []
created_date: '2026-11-01T13:30:00Z'
updated_date: '2026-11-01T19:49:00Z'
labels:
  - needs-mark
  - safety
  - policy
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Merged 2026-11-01 as the implementer's/orchestrator's choice, marked 'pending Mark's confirmation' in PRD and safety.md. (1) ensure (incl. --dry-run) and reclaiming watch refuse to start, exit 2 cause safety, under hidepid or outside the initial PID namespace, when a selected tier uses diskgc's own in-use scan (fs incl. command actions, cargo-target); docker/nix-store tiers still run; a hidepid gid= group diskgc belongs to is honoured. Effect: diskgc inside a CI job container exits 2 where 0.1.0 ran (run it on the host instead). Alternative: report every candidate in_use_unknown (exit 1, looks like a busy host), or an explicit logged operator opt-in. (2) udisks does NOT chown /dev/loopN to the user (verified in udisks 2.11.2 source), so a user outside the disk group whose own image is attached via udisks cannot open the device; diskgc then cannot prove which file backs it and blocks every fs/cargo candidate (in_use_unknown, unknown_cause loop_device) until detached. Options: accept; add the user to disk (security cost); narrow by the resolved name's filesystem accepting the decoy-overmount residual (needs root to set up in diskgc's namespace). Remove the 'pending' markers once decided.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01 (TASK-298 merged): under hidepid / outside the initial PID namespace, a reclaiming ensure with the goal already met exits 0 (stderr warning that a reclaim would be refused); only a run below --min-free, the dry run and a reclaiming watch are refused. CI pre-job hooks in containers pass while space is there.
<!-- SECTION:NOTES:END -->
