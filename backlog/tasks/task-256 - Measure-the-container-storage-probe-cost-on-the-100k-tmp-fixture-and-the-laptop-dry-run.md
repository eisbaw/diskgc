---
id: TASK-256
title: >-
  Measure the container-storage probe cost on the 100k /tmp fixture and the
  laptop dry run
status: To Do
assignee: []
created_date: '2026-10-31T00:32:00Z'
labels:
  - performance
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-242 adds statx probes for container-storage markers: about 10.4 statx per directory checked, repeated over the root's ancestors and the path to each candidate at approach, recheck under the lock and before acting (313 of 440 statx in one fs race vector trace). Measure time to first deletion and total run time on fs_reclaim_100k_entry_time_to_first_deletion_is_measured_cold_and_warm before/after 9937817 and a host read-only pytest-tmp dry run; if material, cache the per-run answer for the root's ancestors (a marker created above the root mid-run is a multi-event case) or probe once per directory per run. Needs disk >= 15 GiB.
<!-- SECTION:DESCRIPTION:END -->
