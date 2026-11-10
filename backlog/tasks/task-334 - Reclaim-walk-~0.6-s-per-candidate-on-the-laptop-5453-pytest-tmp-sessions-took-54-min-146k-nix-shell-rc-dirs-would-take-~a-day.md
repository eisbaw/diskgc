---
id: TASK-334
title: >-
  Reclaim walk ~0.6 s per candidate on the laptop: 5,453 pytest-tmp sessions
  took 54 min, 146k nix-shell-rc dirs would take ~a day
status: In Progress
assignee: []
created_date: '2026-11-04T14:58:00Z'
updated_date: '2026-11-10T17:46:00Z'
labels:
  - performance
  - dogfood
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Dogfood 2026-11-04 on Mark's laptop, release-candidate 0.2.0-dev (4bea92c, /nix/store/5hpip78kgrwb33rln9vzjh4l0d350rm3-diskgc-release-candidate-0.2.0-dev), non-root, per-user lock, ext4 on LUKS, 14 cores, two cargo builds running alongside.

1. Dry run: ensure --dry-run --json --min-free 200G --select 'profile:pytest-tmp || profile:nix-shell-rc' /tmp: exit 0 in 383 s; 152,829 candidates; pytest-tmp 6,090 eligible (11.0 GiB est.), nix-shell-rc 146,136 eligible (0.83 GiB est.).
2. Same args without --dry-run (default --timeout 10m): exit 124 after 600 s; tally budget spent; 585 pytest sessions deleted (avail delta 0.75 GiB); per-action duration_ms summed to only 66 s.
3. --select profile:pytest-tmp --timeout 1h: exit 1 (exhausted, goal unreachable by design) after 3,227 s; synchronous tally 23 s, first deletion at 30 s; 5,453 sessions reclaimed (avail delta 5.5 GiB on the first 1,001 actions listed, net df +5.8 GB with concurrent builds); no errors.

So the reclaim walk runs at about 0.59 s per candidate (3,200 s / 5,453), matching the TASK-203 estimate (0.5 to 1 s per candidate), and most of it is outside the per-action duration_ms (in-use scans under the 1 s freshness bound and the final checks). At that rate the remaining ~146k nix-shell-rc dirs (4 KiB each, 0.83 GiB) would take about a day. The dry-run side was fixed by TASK-325 (batched scans + read-ahead); the reclaim walk was not.

Evidence: tmp/walks/2026-11-04-dogfood/ (a2-*.json/.err, a2b-*.json/.err with per-line timestamps).

Open: profile where the walk spends its time (scan vs walk vs final checks vs the actions JSON cap at 1,001); whether one scan can serve more consecutive tiny fs candidates within FRESHNESS (safety.md In use: Freshness) without weakening it; whether the human/JSON report should say the time spent outside actions.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Mark 2026-11-10: 'do all first four' of the proposed order: (1) profile one reclaim in a container on a large fixture (scan, mountinfo, walk, final checks, report); (2) bring the dry run's batched scan (TASK-325) to the reclaim, inside the existing 500 ms reuse and 1 s freshness bounds; (3) TASK-294's mount-table reuse while poll on /proc/self/mountinfo reports no change; (4) TASK-300's verdict-relevant mount comparison. Step 5 (bytes-per-second ordering for tiny candidates) not chosen. Order after the profile follows what it shows. Starts on the guard stack (t370) once that is committed: both touch executor.rs and fs_kind/reclaim.rs.

2026-11-10 step 1 (profile) done; evidence tmp/t334/. Laptop dogfood a2b: ~77% of the 0.59 s per pytest candidate is record_as waiting up to 2 s (250 ms re-samples) for freed space to show, on nearly every candidate whose freed bytes fell short of the tally estimate; the goal was unreachable so the wait could not change a decision. Reproduced in the container with a writer (15 of 15 candidates waited 2.0 s). The wait also ages the process scan past the 1 s bound, forcing a rescan per waited candidate. Container floor per candidate: rc ~3.2 ms, pytest ~15 ms. Bounds: scan batching <=9% laptop, TASK-294 <0.5% laptop / 13-36% container floor, TASK-300 0 without mount churn. New step 0 (not among the four Mark approved, for his veto): skip the wait when the outstanding shortfall could not reach the goal. Order now: step 0, TASK-294, re-measure, scan batching, TASK-300. Unexplained: nix-shell-rc a4 ~115 ms per candidate outside the actions.

2026-11-10 step 0 merged to master in 177297c0 (commit 6d2a3572, local). Rule narrowed from the first idea: the wait is skipped only in tiers that decide by the goal alone (oldest order, delete or Cargo protocol) when expected space stays below target; tiers that read the remaining need (best-fit, commands, Nix --max-freed, prunes) always wait; final sample waits once; an exhausted 2 s window resets the expectation. Container E3: 42.1 s -> 9.8 s, 16 two-second waits -> 0, scans 25 -> 9, same 1,500 reclaimed. Observable changes for Mark's veto: avail_delta_bytes attribution can shift between actions; --timeout/signal runs get further (may exit 0 where 124 before). Gates: check 0, e2e 0. Next: TASK-294, re-measure, scan batching, TASK-300.
<!-- SECTION:NOTES:END -->
