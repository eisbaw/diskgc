---
id: TASK-338
title: >-
  Backend inspection for a dry run / survey: podman info may initialise storage;
  inspection ignores deadline and SIGINT
status: To Do
assignee: []
created_date: '2026-11-04T17:18:00Z'
labels:
  - dry-run
  - backends
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Codex review of survey (2026-11-04), verified by the t38 implementer to predate survey and affect ensure --dry-run on master: (1) a --select profile:podman-* tier's inspection queries podman --remote=false info, which on first use initialises Podman storage/runtime state (not read-only); (2) backend inspection (docker/podman queries, nix binding) receives no deadline and checks no signal between queries; each Docker query waits up to its own 10 s regardless of SIGINT. Fix: a side-effect-free inspection (or refuse until one exists) and propagate the absolute deadline and cancellation through inspection and every query wait.
<!-- SECTION:DESCRIPTION:END -->
