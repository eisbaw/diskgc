---
id: TASK-144
title: >-
  Release binary under host-shaped load: reclaim stops on freshness after
  deleting a Cargo target part-way (exit 3)
status: To Do
assignee: []
created_date: '2026-10-24 21:09'
labels:
  - shipping
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-143 walk (2026-10-24, walk-diskgc-podman, release-01-candidate /nix/store/hzhjlyqp...-diskgc-release-01-candidate-0.1.0, host load 27-31 on 14 cores, container load: tools/walk/load.py 281 procs/2803 tasks + 2 niced busy loops).

Repro: just walk-up && just walk-reset; in the walk shell: diskgc ensure --min-free 2G --select profile:cargo-target --roots /home/tester/src /home/tester/src

Seen: 3 targets reclaimed, then fw-nrf/target 'deleted part-way: 206 entries', then 'error: the run stops: .../fw-nrf/target/debug/build: in-use scan 300 processes: two scans in a row were older than the freshness bound of 1s once finished (the last took 1.283380149s)'; exit 3 at 1.9 of 2.0 GiB. Rerun on the remaining targets (same load) met the goal (exit 0). 1 of 2 runs failed. Pane dumps: tmp/walks/2026-10-24/42-reclaim-unfiltered.txt (fail), tmp/walks/2026-10-24/43-reclaim-unfiltered-2.txt (pass).

User impact: a laptop owner on a busy machine (the PRD's overnight-agents case) gets exit 3 and a half-deleted target; TASK-115 recorded this residual for load above ~18 (13/14). The walk shows it at host load ~28 with only the host-shaped load inside. Decide: mimalloc (branch task115-mimalloc), more allocation cuts, or the freshness design options; measure with just walk-up.
<!-- SECTION:DESCRIPTION:END -->
