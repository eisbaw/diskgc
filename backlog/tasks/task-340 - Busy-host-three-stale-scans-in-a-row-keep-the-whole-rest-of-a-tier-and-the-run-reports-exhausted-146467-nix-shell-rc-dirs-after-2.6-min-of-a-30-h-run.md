---
id: TASK-340
title: >-
  Busy host: three stale scans in a row keep the whole rest of a tier and the
  run reports exhausted (146,467 nix-shell-rc dirs after 2.6 min of a 30 h run)
status: Done
assignee: []
created_date: '2026-11-04T18:54:00Z'
updated_date: '2026-11-05T06:07:00Z'
labels:
  - dogfood
  - freshness
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Dogfood 2026-11-04 16:25, release-candidate 0.2.0-dev (4bea92c), systemd-run --user unit, ensure --json --timeout 30h --min-free 200G --select profile:nix-shell-rc /tmp, load ~20-30 from three parallel cargo/nix builds on the 14-core laptop. Exit 1 status exhausted after 159 s: 135 reclaimed, 146,467 kept in_use_unknown ('not checked: the host was too busy to check the 3 candidates before it in this tier; kept, run again when the host is less busy'), errors []. Evidence: tmp/walks/2026-11-04-dogfood/a3-rc-real.{json,err}.

Safe (nothing deleted on a stale scan), but: (1) with 30 h of deadline left the tier gives up on its whole remainder instead of backing off and retrying (safety.md In use: Freshness, three-in-a-row limit, which exists for a sustained fork storm); (2) exit 1 / status exhausted tells a CI hook or watch nothing more can be reclaimed, while 146k candidates were never judged; the summary puts them under kept, not not_judged. Decide: back-off and retry within the deadline (bounded), and a distinct status/exit or at least not_judged accounting for candidates skipped because the host was busy. Keep the guarantee: never delete on a scan older than FRESHNESS.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-05 orchestrator: implementer in tmp/worktrees/t340 (branch t340-busy-host, from master 6c67eccb).

Merged ec367fca9ecafabed501bea557dbffae37c4c379 (2026-11-05). Open: real-kernel check of the back-off (dogfood nix-shell-rc rerun, dry run first). Follow-ups: human summary counts unreadable_process under 'host too busy' (report/human.rs busy()); configured tier default 10m timeout turns a long storm into exit 3; live-run not_judged count needs design; mixed-cause sequences untested; per-candidate retry pauses still slice-sleep; deadline hint says 'larger --timeout' for a busy host.
<!-- SECTION:NOTES:END -->
