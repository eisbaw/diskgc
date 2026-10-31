---
id: TASK-262
title: >-
  watch: exclude a second watcher across btrfs subvolumes of one filesystem;
  review leftovers
status: To Do
assignee: []
created_date: '2026-10-31T01:54:00Z'
labels:
  - watch
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the TASK-32 review (Codex 10, mped N1): (1) watcher exclusion and the snapshot key use st_dev, so two watchers on different btrfs subvolumes of one filesystem both start (host lock still serialises deletions; double tally). Key on the mountinfo superblock device (TASK-30's space guard already does); the model needs subvolume support (a file st_dev differing from its mount) to test it. (2) A device-mismatch cycle record has no available-space numbers although the sample succeeded. (3) No test that a slow index wait eats into the cycle deadline (the inline model driver never waits).
<!-- SECTION:DESCRIPTION:END -->
