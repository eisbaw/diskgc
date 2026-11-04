---
id: TASK-36
title: 'nix-store kind: store selection check and bounded GC'
status: In Progress
assignee: []
created_date: 2026-10-16 22:41
updated_date: '2026-11-04T03:34:00Z'
labels:
  - wave-4
  - irreversible
  - safety
dependencies:
  - TASK-118
references:
  - docs/safety.md#nix-store
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: Nix stores grow without bound on development machines. Implement nix-store per docs/safety.md#nix-store: store-in-effect check with the exact environment, supported store URLs, nix-store --gc --max-freed, measured effect; container with its own store and daemon.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 GC removes only dead paths in the container store; roots and generations survive
- [ ] #2 Killing diskgc while nix-store runs leaves the host lock held until it exits
- [ ] #3 A remote or non-default store, a single-user layout, or a non-allowlisted Nix version is skipped per docs/reference.md 'Backend skips and errors' (remote_backend / unsupported_backend), never GC'd
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-50 (sandbox gate): starting a prune, GC or command child is a deleting operation (docs/safety.md, Pre-release sandbox gate). Put it on diskgc_core::host::Delete, not Host. In LinuxHost (crates/diskgc-linux/src/host.rs), take a Permit from gate(op, subject) first and pass it to a new *_permitted wrapper that is the only place the child is started; the source scan deleting_syscalls_only_behind_the_gate forbids Command/spawn/fork/exec anywhere else in diskgc-linux. Extend its allowlist by that one wrapper, do not weaken it. A refusal is HostError with cause Cause::SandboxGate (kernel_errno() None): report it as an error, never as a skip or a retry. ModelHost implements the method without a gate.

CORRECTION from task-50's review (supersedes the earlier task-50 note that every child start is deleting and belongs on Delete; that note contradicts docs/safety.md and the PRD). There are two child-process operations (docs/architecture.md, The Host port):
(1) A query run on Host: not gated, so survey, --dry-run and --tally-only work anywhere. Examples: docker context/builder/info and the storage-root queries, nix store info --json in the store-in-effect preflight. Its argv comes from constants in the code, never from config or profiles.
(2) A deleting run on Delete: gated by the pre-release sandbox gate. Examples: docker builder prune / image prune, nix-store --gc.
In LinuxHost each gets exactly one wrapper. The deleting one takes a Permit from gate(op, subject); the query one takes none but accepts only the constant argv set. Add both to the allowlist of the scan in crates/diskgc-linux/src/source_scan.rs (GATED_ALLOWED; the scan forbids Command, fork and exec everywhere else) without weakening its pattern list. A gate refusal is Cause::SandboxGate and must be reported as an error, never as a skip or a retry. ModelHost implements both without a gate.

From task-50 round 3 (mped): the query run takes a closed enum of queries that diskgc-linux maps to argv (docs/architecture.md, The Host port). It never takes argv from the caller or from config. Its LinuxHost wrapper is a GATED_ALLOWED entry marked Gate::Ungated with its reason; the deleting run's wrapper is marked Gate::Permit (checked: its signature must take the Permit).

2026-10-20 disposable-container Nix 2.34.8 feasibility proof: NIX_REMOTE=local with NIX_STATE_DIR=/tmp/state-A and /tmp/state-B reports the same store info {url: local, version: 2.34.8}; builtins.storeDir remains /nix/store while the root databases differ. NIX_STORE_DIR can also change without altering URL. The daemon URL does not attest the physical store or socket target. Therefore URL plus logical store-dir checks cannot establish the GC root set, and a GC could delete paths that are live in the actual root DB. No nix-store profile shipped. docs/safety.md records the physical store plus root database binding requirement. Need a verifiable binding protocol or strictly pinned local configuration with independent root DB proof, then ModelHost and disposable-store E2E. No host GC or project path tested.

2026-10-20 disposable Nix 2.34.8 container: an explicit direct-local store URL with store=/nix/store, real=/tmp/real and state=/tmp/state-a overrode conflicting NIX_REMOTE and NIX_CONFIG for root query and bounded GC. A root in state A survived its GC; registering the same path in state B on the same physical real directory and collecting through B deleted the path still rooted in A. The child URL pins only its selected database, not the daemon database or exclusive ownership of the physical store. No nix-garbage profile shipped. A safe implementation needs a pinned daemon connection with independently verified physical-store and root-database identity held through GC, or a proven exclusive direct-local deployment, plus ModelHost and disposable-container tests. No host GC was run.

Clarification of the preceding state-A/state-B experiment: state B recorded the same physical store path as a valid store path, without adding a GC root in B. The GC root for that path existed only in state A. GC through B therefore removed it despite the root in A; no claim is made that GC removed a path rooted in B.

Orchestrator 2026-10-23 (design audit): now depends on TASK-118 (store/root-db binding protocol design).

Orchestrator 2026-10-24 (COMPASS round 2): dropped the TASK-34 edge (nix-store does not depend on index hardening). Naming: settle skip-vs-error and unsupported_store vs unsupported_backend across all backends inside TASK-118 (Mark ruled refuse with unsupported_backend; Docker client failures are skips per TASK-120).

From TASK-118 (2026-10-24, branch task118-design; design in docs/safety.md nix-store, reference.md 'Backend skips and errors' and 'Backend version allowlist', testing.md 'nix-store backend vectors' + 'Nix GC' container scenarios). Implementation plan, bottom-up:
1. Host port (diskgc-core host.rs): closed enum NixQuery {ClientVersion, ConfigShowStore, StoreInfoPinned, } on Host (ungated), mapped to constant argv in LinuxHost; every 'nix' argv carries --extra-experimental-features nix-command; exec '<store path>/bin/nix-store' or '<dir>/bin/nix' (argv[0] keeps multi-call dispatch), never PATH again. New ungated query nix_daemon_probe(pinned fd): connect, send WORKER_MAGIC_1, read WORKER_MAGIC_2 within 10 s, close -> Serving | Eof | Timeout | WrongMagic | Errno. Socket pin reuses the Docker pattern (O_PATH|O_NOFOLLOW, fstat, /proc/self/fd/N inherited). Deleting runs on Delete (gated, Permit): NixRun {QueryRoots(targets), Gc{max_freed}}; env exactly HOME + NIX_USER_CONF_FILES=''; inherit host lock; own process group; SIGTERM->10 s->SIGKILL. Extend the source-scan allowlist by those wrappers only.
2. Pure binder diskgc-core nix_backend.rs (mirror docker_backend.rs: Binding::{Bound,Skipped}, reasons as in safety.md steps 1-6), plus the allowlist constants (two lists: daemon versions, client --version lines; both EMPTY until step 5 passes). Root check (step 8) pure: generation-link selection from listings (<=4,096 entries per dir, <=1,024 links) and exact '<link> -> <target>' matching of --query --roots output.
3. Executor: kind nix-store loads ({kind, find:{roots: daemon}} only), refused under --allow-root at static invocation (exit 2); preflight binds; reclaim does recheck (changed), root check, GC with need = goal - fresh sample (>=1), one GC per tier, action candidate /nix/store, estimated_bytes null. backend_unavailable becomes a produced reason: drop it from REASONS_WITHOUT_PRODUCER (profile_vectors.rs).
4. ModelHost 'nix:' block + the vectors listed in testing.md (each must turn red when its step is removed).
5. Container image with pinned CppNix 2.34.8 daemon (own /nix, diskgc uid 1000) and the listed scenarios; confirm in it: --query --roots output format for direct and per-user generation links, handshake EOF on allowed-users refusal and on a failed socket-activated service, the claimed GC semantics (max-freed stop, .links phase after limit, hang-up interruption), whether a trusted client forwards system-file settings, and the root check's duration. Only then add 2.34.8 to both allowlist lists and lift the loader refusal.
AC #2 is superseded by the one backend rule (reference.md): NIX_REMOTE pointing elsewhere is a SKIP, remote_backend for remote schemes and unsupported_backend otherwise, exit decided by space; 'unsupported_store' no longer exists. Suggest rewording AC #2 accordingly (not edited here: TASK-118 may only append notes to this task).

Orchestrator 2026-10-24: AC #2 text ('unsupported_store reported as an error') is superseded by TASK-118: unsupported_store is removed from the JSON enum; remote stores skip with remote_backend, unsupported layouts/versions with unsupported_backend, per the backend skip-vs-error rule in docs/reference.md. Rewording AC #2 accordingly.

2026-11-03: Mark: 'nix-store goes first then the visibility'. In progress in tmp/worktrees/t36 (branch t36-nix-store). Full rigour (new irreversible kind), not the one-round profile rhythm.

2026-11-04 Mark: 'i dont care about lix'. Lix and other Nix variants stay refused (unsupported_backend, version allowlist); the planned pinned-Lix container scenario is dropped; the model vector nix-lix-client-refused stays as the cheap guard.

2026-11-04 implementer, review round 1 (Codex + mped on 09ed55e), fixed in 49e1b3d on t36-nix-store (rebased on 76571ef): client anchored to the installation (/run/current-system/sw, root's default profile; links root's), ELF nix in a root-owned read-only <hash>-nix-<admitted> store path, exec through the checked descriptor; --option plugin-files '' on every Nix argv; config show store in diskgc's cwd; no-deadline overflow fixed; profiles --json where/checkouts/backend; narrowed container oracle plus link identity checks; S1-S6 and NITs (see commit). 52 nix-* vectors, 27 bites (all red but step1-store-bin, subsumed by the name check); 14 container scenarios. Gates: nix develop -c just check exit 0; nix develop -c just e2e exit 0 on 49e1b3d (nix 246/246, release 11/11, release-debian 11/11, docker-dind 7/7, nix-gc 14/14, VM checks). Host walk read-only: dry run binds through /run/current-system (eligible, estimated_bytes null); nix_dead.py still skips (dangling gcroots/auto registration). Not merged.
<!-- SECTION:NOTES:END -->
