---
id: TASK-130
title: >-
  Vector runner: config parsing, release catalog and account lookup through one
  product path
status: To Do
assignee: []
created_date: '2026-10-23 22:39'
labels:
  - verification
  - architecture
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-121 review (mped-architect) findings 1, 5, 6. (1) main.rs load_config parses the config and, under feature proposed-release-01-catalog, runs enforce_proposed_release_01_config; the vector runner calls config::parse itself and synthesises the exit-2 report, and no vector can run the release catalog that would ship. Move 'config bytes -> Config' with an explicit catalog value (Development | ProposedRelease01) into diskgc_core::invocation, route its refusal through ensure's single refusal path, and let a vector say catalog: proposed-release-01. (5) The 'no configuration and no --select' refusal is decided twice (main.rs ensure_config with the tried paths, and invocation::run); keep one owner. (6) Account lookup (owners: names) is a machine read outside the Host port; move it onto the port so vector 'users' becomes host-model state. Reversible; no deletion decision changes.
<!-- SECTION:DESCRIPTION:END -->
