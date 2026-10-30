---
id: TASK-248
title: Bound Docker backend inspection by the overall ensure deadline
status: To Do
assignee: []
created_date: '2026-10-26T23:14:00Z'
labels:
  - bug
  - hardening
dependencies: []
references:
  - crates/diskgc-core/src/docker_backend.rs
  - crates/diskgc-linux/src/host.rs
  - crates/diskgc-core/src/executor/plan.rs
priority: medium
type: bug
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Review finding P2 (2026-10-27): Docker backend inspection can exceed ensure's overall timeout by starting successive read-only queries after that deadline.

Root cause: crates/diskgc-core/src/docker_backend.rs inspect_backend/query do not receive a run deadline or check cancellation between queries. LinuxHost::docker_query in crates/diskgc-linux/src/host.rs gives each child its own DOCKER_QUERY_TIMEOUT of 10 seconds. Executor reclaim and dry-run paths check the overall deadline only after the complete inspection returns.

Reproduced against the development CLI with a fake Docker executable on PATH: each of context show, context inspect, server version, client version and info slept four seconds, then returned valid responses. No real Docker daemon or deletion was used. Command:
nix develop -c env PATH="$PWD/tmp/review-probe/bin:$PATH" tmp/target/debug/diskgc ensure --dry-run --json --config "$PWD/tmp/review-probe/config.yaml" --select profile:docker-build-cache --timeout 1s --min-free 0K /

The config contained only version: 1. Result: exit 124, duration_ms 20737, preflight over_ms 19736, with no kernel-blocked filesystem operation. The stub returned default context, unix:///var/run/docker.sock, Docker Engine - Community / version 29.8.2, and a local /var/lib/docker storage root.

Propagate the remaining run deadline through inspection and query execution, and check deadline/interruption before subsequent queries. Preserve backend binding checks and fail-closed behavior. Scope: development Docker backend, outside the proposed 0.1 catalog.

Local reproduction artifacts (ignored scratch, not permanent regression coverage): tmp/review-probe/bin/docker, tmp/review-docker-deadline.json, tmp/review-docker-deadline.log.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Backend inspection receives the run deadline, starts no query after it expires, and caps each query wait by the remaining time.
- [ ] #2 A deterministic regression with individually successful slow Docker queries fails on the old implementation and proves the sequence cannot multiply the overall timeout; cover dry-run and reclaim callers.
- [ ] #3 Interruption stops subsequent queries and cleans up a running query child without weakening daemon identity or storage checks.
- [ ] #4 Add an isolated CLI/container regression using a fake Docker client; verify the deadline result and no prune invocation, and run just e2e before committing.
<!-- AC:END -->
