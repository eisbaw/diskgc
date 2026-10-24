---
id: TASK-154
title: >-
  Walk driver: old ctime cohorts (idle-1d and longer) need a VM-backed walk
  environment
status: To Do
assignee: []
created_date: '2026-10-24 21:12'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-143. The Podman walk container (just walk-up) generates data whose kernel ctime is the generation time; diskgc's idle measure is the newer of mtime and ctime, and a rootless container cannot set the clock, so only idle-1h becomes true (an hour after walk-reset). The laptop owner's idle-7d/idle-30d tiers and the CI hook's idle-1d default can only be walked as all-not_idle. Option: a walk-up ENV=vm variant reusing the vm-historical-ctime machinery (guest clock set back while generating, private ext4 disk = also a real block filesystem), with the same tools/walk scripts inside and the serial console in the tmux session.
<!-- SECTION:DESCRIPTION:END -->
