---
id: TASK-35
title: 'Docker kinds with one bound backend: build cache and images'
status: In Progress
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-20 05:50'
labels:
  - wave-4
  - irreversible
  - safety
dependencies:
  - TASK-34
  - TASK-57
  - TASK-104
references:
  - docs/safety.md#docker-kinds
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: Docker caches are large and the prune commands delete broadly. Implement backend binding per docs/safety.md#docker-kinds (local system socket only, default builder with docker driver, storage roots incl. the containerd store root) used by tally, preflight and execution; build-cache prune with --all and until; dangling and unreferenced image prune; settle containerd-root discovery.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 dind container tests: remote context, rootless socket and non-default builder are skipped with their reasons
- [x] #2 Volumes and containers are never removed (asserted before/after)
- [x] #3 The containerd storage root is found with the method from the spike; undeterminable setups are unsupported_backend
- [x] #4 The dind image is pinned by hash (offline), runs privileged with a tmpfs for /var/lib/docker
- [x] #5 Killing diskgc while docker prune runs leaves the host lock held until the docker client exits
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-50 (sandbox gate): starting a prune, GC or command child is a deleting operation (docs/safety.md, Pre-release sandbox gate). Put it on diskgc_core::host::Delete, not Host. In LinuxHost (crates/diskgc-linux/src/host.rs), take a Permit from gate(op, subject) first and pass it to a new *_permitted wrapper that is the only place the child is started; the source scan deleting_syscalls_only_behind_the_gate forbids Command/spawn/fork/exec anywhere else in diskgc-linux. Extend its allowlist by that one wrapper, do not weaken it. A refusal is HostError with cause Cause::SandboxGate (kernel_errno() None): report it as an error, never as a skip or a retry. ModelHost implements the method without a gate.

CORRECTION from task-50's review (supersedes the earlier task-50 note that every child start is deleting and belongs on Delete; that note contradicts docs/safety.md and the PRD). There are two child-process operations (docs/architecture.md, The Host port):
(1) A query run on Host: not gated, so survey, --dry-run and --tally-only work anywhere. Examples: docker context/builder/info and the storage-root queries, nix store info --json in the store-in-effect preflight. Its argv comes from constants in the code, never from config or profiles.
(2) A deleting run on Delete: gated by the pre-release sandbox gate. Examples: docker builder prune / image prune, nix-store --gc.
In LinuxHost each gets exactly one wrapper. The deleting one takes a Permit from gate(op, subject); the query one takes none but accepts only the constant argv set. Add both to the allowlist of the scan in crates/diskgc-linux/src/source_scan.rs (GATED_ALLOWED; the scan forbids Command, fork and exec everywhere else) without weakening its pattern list. A gate refusal is Cause::SandboxGate and must be reported as an error, never as a skip or a retry. ModelHost implements both without a gate.

From task-50 round 3 (mped): the query run takes a closed enum of queries that diskgc-linux maps to argv (docs/architecture.md, The Host port). It never takes argv from the caller or from config. Its LinuxHost wrapper is a GATED_ALLOWED entry marked Gate::Ungated with its reason; the deleting run's wrapper is marked Gate::Permit (checked: its signature must take the Permit).

2026-10-20 forward-carried from TASK-104: native Docker Engine dind fixture must exist before Docker-kind E2E counts; the host docker CLI reports Podman 5.8.7. No developer host Docker socket or project paths may be used for testing.

2026-10-20 implementation in isolated worktree: native Docker Engine 29.8.2 dind exercises image pruning, dangling-image pruning and BuildKit cache pruning, preserving containers/volumes; a pinned Unix socket descriptor prevents pathname-swap redirection and inherits the host lock through CLI completion. Final CLI-version support binding, rebase onto 8a84934, full just e2e, and final QA/MPED review remain before commit. No host Docker daemon or project path tested.

2026-10-20 commit bc37668: Docker build-cache, dangling-image, and unreferenced-image profiles shipped for a bound local Docker Engine. Exact-tree nix develop -c just e2e passed 127/127 Nix and 5/5 native Docker dind; final QA and MPED GO. Native tests prove actual cache/image removal, container/volume preservation, side Buildx builder cannot redirect Engine prune, socket replacement refusal, and host lock inheritance after parent kill. Build-cache is deliberately limited to verified Docker CLI 29.8.2; other client versions skip unsupported_backend. No host Docker daemon or project path was used. TASK35 stays In Progress because AC1 literally asks to skip a non-default builder, whereas the implementation forces direct Engine dispatch with DOCKER_BUILDKIT=0 and safely prunes despite a selected side builder; reconcile that acceptance criterion before marking Done. Remote/rootless negative binding is covered in ModelHost.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
