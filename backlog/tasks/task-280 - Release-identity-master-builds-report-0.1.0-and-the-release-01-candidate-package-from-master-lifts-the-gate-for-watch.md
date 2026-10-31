---
id: TASK-280
title: >-
  Release identity: master builds report 0.1.0 and the release-01-candidate
  package from master lifts the gate for watch
status: To Do
assignee: []
created_date: '2026-10-31T22:47:00Z'
labels:
  - release
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
COMPASS 2026-10-31: /nix/store/cgldrwb...-diskgc-release-01-candidate-0.1.0 built from master reports 'diskgc 0.1.0' and its help lists a reclaiming watch with the deletion gate lifted, although no release includes watch (TASK-276 undecided). Evidence from master builds (incl. the TASK-277 host dogfood) cannot be told apart from the tagged 0.1.0. Fix: master version 0.2.0-dev; the candidate package from master named for the next release (or the 0.1 candidate built only from release-0.1); a git sha in --version for non-tagged builds; the release catalog/gate decision for watch made explicit (TASK-276). Also: docs/reference.md '0.1 release notes' still say 'Status: draft ... not released until TASK-67 closes' although v0.1.0 is tagged.
<!-- SECTION:DESCRIPTION:END -->
