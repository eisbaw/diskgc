---
id: TASK-113
title: >-
  Classify Docker client query failures: Podman CLI shim, daemon down, socket
  EACCES
status: To Do
assignee: []
created_date: '2026-10-23 15:48'
updated_date: '2026-10-23 21:01'
labels:
  - safety
  - docker
dependencies: []
references:
  - docs/safety.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-35's read-only host dry run (2026-10-23): on this laptop `docker` is Podman 5.8.7's CLI shim; `diskgc ensure --dry-run --select profile:docker-build-cache /var/lib/docker` (also docker-dangling, docker-unreferenced) exits 3, status error: 'query Docker docker ["context", "show"]: exited exit status: 125: Error: unrecognized command'. docker_backend::query maps only ENOENT to a skip (missing); every other client failure is a HostError (exit 3). The class is wider than Podman: a Docker client without a context subcommand, a stopped daemon, EACCES on the socket. A timer-driven ensure on a Fedora/podman-docker host would exit 3 every run. Fails closed today (documented in docs/safety.md, Docker kinds). Decide per case, by narrow identification (never a blanket error-to-skip conversion that hides a real daemon failure), which are skips (unsupported_backend / missing) and which remain errors; note watch's 1-minute backend back-off in docs/reference.md.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each listed client failure case has a documented classification (skip reason or error) in docs/safety.md with its rationale
- [ ] #2 ModelHost cases (YAML fixture variants of models/docker-bundled.yaml) pin each classification and turn red when the identification is removed
- [ ] #3 A read-only dry run on a Podman-shim host is recorded with command, exit code and report status
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Orchestrator 2026-10-23 (design audit): TASK-120 (Docker contract leftovers design) resolves the skip-vs-error split together with the Buildx sizing conflict; coordinate.

Mark's design decisions (2026-10-23, binding): client failures before a backend query become SKIPS with reason, not exit 3 (see TASK-120 notes).
<!-- SECTION:NOTES:END -->
