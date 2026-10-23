---
id: TASK-35
title: 'Docker kinds with one bound backend: build cache and images'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-16 22:41'
updated_date: '2026-10-23 15:57'
labels:
  - wave-4
  - irreversible
  - safety
dependencies:
  - TASK-57
  - TASK-104
references:
  - docs/safety.md#docker-kinds
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: Docker caches are large and the prune commands delete broadly. Implement backend binding per docs/safety.md#docker-kinds (local system socket only, verified direct Engine builder dispatch independent of Buildx selection, storage roots incl. the containerd store root) used by tally, preflight and execution; build-cache prune with --all and until; dangling and unreferenced image prune; settle containerd-root discovery.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Volumes and containers are never removed (asserted before/after)
- [x] #2 The containerd storage root is found with the method from the spike; undeterminable setups are unsupported_backend
- [x] #3 The dind image is pinned by hash (offline), runs privileged with a tmpfs for /var/lib/docker
- [x] #4 Killing diskgc while docker prune runs leaves the host lock held until the docker client exits
- [x] #5 dind container tests: remote context and rootless socket are skipped with their reasons; a selected non-default Buildx builder cannot redirect the pinned Engine builder prune
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

2026-10-20 acceptance correction: verified Docker CLI 29.8.2 dispatch forces direct Engine BuildCachePrune with DOCKER_BUILDKIT=0; a nondefault Buildx builder is selected in dind but cannot redirect the action. Thus AC1 now requires its non-redirection rather than an unnecessary skip. The same native test now exercises a configured remote context and a rootless Unix socket as skipped, with no prune and a clean deletion oracle. ModelHost classifies a non-system Unix override as unsupported_backend. Focused native Docker test passed 1/1; full gate and QA/MPED remain before Done.

2026-10-20 acceptance verification: native Docker dind normal ensure (not dry-run) rejects a configured named remote context and a rootless Unix socket as unsupported_backend; each yields a nonempty skip, zero actions, exit 1 exhausted, and preserves a nonempty BuildKit cache ID set. Disposable deletion oracle is clean. Pinned Engine prune still operates independently of selected side Buildx builder. Focused native dind test passed 1/1; ModelHost docker_binding passed 7/7. Full exact-tree gate remains before DoD/status closure.

2026-10-20 mutation trial in throwaway /tmp/diskgc-task35-mutant: changed inspect_backend non-system Unix DOCKER_HOST classification to remote_backend; remote_rootless_and_external_store_fail_closed failed (exit 101, expected unsupported_backend). Restored intended code in TASK35 worktree passed docker_binding 7/7. DoD remains open: docs/testing.md requires a real-machine read-only dry run for a changed shipped profile, but the user explicitly prohibits running diskgc on the real host. No such run was performed. TASK35 remains In Progress pending an authorized safe way to satisfy or revise that criterion. Full exact-tree gate is being rerun after formatting.

2026-10-20 combined gate on aab98ce: nix develop -c just e2e passed fmt, clippy, workspace tests, Nix checks, 129/129 Nix container E2E and 5/5 native Docker dind E2E. QA and MPED GO on the restacked staged diff. TASK35 remains In Progress solely because its docs/testing.md real-machine dry-run DoD is prohibited by the user host-testing constraint; no host diskgc run was done.

Orchestrator 2026-10-23 (COMPASS consult): dropped the TASK-34 edge (a built feature must not depend on a later hardening wave). Mark ruled 2026-10-23 that read-only diskgc (--dry-run/check/survey/profiles) MAY run on the developer host, so the DoD real-machine dry run is permitted.

2026-10-23 DoD close-out. (1) Daemon-backed dry run: docs/testing.md (DoD, daemon-backed profiles) puts it on the real daemon in a disposable container. Native dind tests in diskgc-e2e/src/docker_suite.rs run `ensure --dry-run --json --no-index --min-free 100% --select profile:P /var/lib/docker` for P = docker-unreferenced (native_docker_image_prune_preserves_containers_volumes), docker-dangling and docker-build-cache (native_docker_dangling_and_build_cache_prune): each exit 0, plan of exactly one whole-backend entry with reason null, oracle 0 traced deleting calls. Latest pass: today's Docker stage (tmp/e2e-logs-docker, 2026-10-23 15:11, 7/7) on code identical to HEAD. Sizes are not reported: the plan entry is a whole-backend action. (2) Real-host read-only dry run (Mark's 2026-10-23 ruling), GNU debug build of HEAD source: `nice -n 19 ionice -c3 tmp/target/debug/diskgc ensure --dry-run --json --no-index --timeout 10m --min-free 100% --select profile:P /var/lib/docker` for all three: exit 3, status error, 0 plan, 0 actions, 20 ms: 'query Docker docker ["context", "show"]: exited exit status: 125: unrecognized command'. This host's docker is Podman 5.8.7's CLI shim, no /var/run/docker.sock. Fails closed; client query failures other than ENOENT are errors by design. Documented in docs/safety.md (Docker kinds); classification filed as TASK-113. (3) New ModelHost case podman_compatibility_server_at_the_system_socket_is_unsupported (docker-bundled.yaml with Platform.Name Podman Engine): unsupported_backend 'not Docker Engine' after exactly 3 queries, both storages. Mutation: disabling the starts_with("Docker Engine") check turned it red (1 failed of 8); reverted, 8/8. Earlier this check had no product-side test. (4) Deep review: QA/MPED GO 2026-10-20 at aab98ce; MPED GO today on the close-out diff. verified_with is set for all three profiles. Gate: just check exit 0 (952 passed, 1 ignored). Remaining limit: Docker kinds untested on a real non-dind Docker host.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
docker-build-cache, docker-dangling and docker-unreferenced bind one local system Docker Engine (default context, system socket pinned by fd, server identity, storage roots incl. the bundled containerd root) and prune it via fixed Engine commands, with containers and volumes preserved, proven in a digest-pinned dind fixture and in ModelHost. The real-host dry run on a Podman-shim laptop fails closed with exit 3 (TASK-113 classifies such client failures).
<!-- SECTION:FINAL_SUMMARY:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
