---
id: TASK-289
title: >-
  Release descriptor: one table per release (commands, profile narrowing,
  shipped files); release-candidate package ships no units, help advertises a
  refused watch
status: To Do
assignee: []
created_date: '2026-11-01T05:37:00Z'
labels:
  - release
  - refactor
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #2 on master 794e0e1 (2026-11-01; Codex tmp/reviews/codex-full2-794e0e1.md + mped), static reading, not reproduced. Codex 2 + mped SHOULD 5/6 + refactor 2: the NixOS module says a reclaiming instance needs a release package, but release-candidate (flake installShippedFiles) ships no diskgc-watch@.service templates, so the module's drop-ins sit on a missing unit (Codex verified with an isolated systemd-analyze fixture); its catalog refuses a reclaiming watch anyway; main.rs HELP/WATCH_SYNOPSIS advertise a reclaiming watch in every build. The release-notes draft says the index is 'not in the release catalog' though the candidate tallies and writes index files; Changes since 0.1.0 omits TASK-272 (exit 3->2), TASK-280, TASK-270, 8edb9f5 and 63c49aa (protected/negated Cargo target fixes: check 0.1.0 exposure, maybe TASK-276 advisory). Fix: a release descriptor in core (allowed commands, profile narrowing, shipped files) with one 'release-catalog' cargo feature; main.rs help, flake installShippedFiles, a NixOS module assertion that the package provides both templates, and the release notes all read from it.
<!-- SECTION:DESCRIPTION:END -->
