---
id: TASK-322
title: >-
  Units VM: the release-candidate artifact's diskgc-watch@ templates under a
  live systemd (PID 1), plus an install-and-upgrade walk from 0.1.0
status: To Do
assignee: []
created_date: '2026-11-02T15:02:00Z'
labels:
  - release
  - vm
  - deployment
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
COMPASS consult #4 (2026-11-02): a 0.2 release-criteria item (docs/architecture.md, Release procedure) had no carrier task. vm-nixos-module runs the development package only; nothing runs the release-candidate package's shipped unit templates under systemd as PID 1. Build a VM that installs the gate-lifted artifact's files (not NixOS module drop-ins), enables a user and a system diskgc-watch@ instance (tally-only per the 0.2 row; refusal of a reclaiming watch checked), checks hardening via systemctl show, SIGHUP/SIGTERM, journal reasons, the tmpfiles host lock, and the per-user lock without it. Plus the install/upgrade walk: install 0.1.0's package, then upgrade to the RC (state dir, snapshot names, lock files, units), README steps followed literally. Depends on Mark's TASK-276 scope (whether a reclaiming watch ships). New infrastructure, not just a run; cleans up per TASK-311.
<!-- SECTION:DESCRIPTION:END -->
