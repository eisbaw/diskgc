---
id: TASK-118
title: 'Design: nix-store store and root-database binding protocol'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-10-23 16:19'
updated_date: '2026-10-24 08:24'
labels:
  - irreversible
  - design
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Design before implementation (audit 2026-10-23 of docs vs PRD). Deliverable is docs text, reviewed by mped-architect, not code. docs/safety.md nix-store says the planned 'nix store info --json' URL check is insufficient and the kind remains unshipped (TASK-36). The PRD lists kind names/schema as hard to walk back while the schema ships as provisional. Design: how diskgc binds one store + its root database (and refuses otherwise), the GC-root/profile/generation never-removed guarantee, --max-freed as a lower bound and overshoot vs the free-space goal, the tally size source for Nix units, Lix/variant handling, test scenarios in a container.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 docs/safety.md nix-store has a complete binding protocol and failure modes
- [ ] #2 docs/reference.md nix-store schema fields are final (not provisional)
- [ ] #3 docs/testing.md names the nix-store scenarios
- [ ] #4 mped-architect design review GO recorded in notes
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Read PRD, safety/reference/architecture/testing Nix+Docker sections, TASK-36/98/127/120/113, docker_backend.rs and vectors.
2. Read-only host facts (nix version, layout, socket, config, pinned-fd connect); read Nix 2.34.8 gc.cc/daemon.cc for --max-freed, links phase, hangup interrupt.
3. Write: safety.md nix-store binding protocol (preflight vs before-GC, root-database canary, never-removed guarantee, max-freed/overshoot, lock/deadlines/SIGTERM, optimise lag, failure modes); reference.md final nix-store schema, allowlist, one backend skip-vs-error rule, enum (remove unsupported_store, add backend_unavailable) + the two test lists that pin the enum; architecture.md Nix sized only at action time; testing.md ModelHost vectors + container scenarios.
4. docs-check + check_docs.py; codex adversarial review; mped-architect review; fold blockers; commit on task118-design.
5. Forward notes to TASK-36 (implementation plan, AC2 superseded), TASK-98, TASK-127.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Mark's design decisions (2026-10-23, binding): (1) Privilege and binding: diskgc runs the Nix GC AS ITS OWN USER VIA THE DAEMON SOCKET (/nix/var/nix/daemon-socket), against /nix/store ONLY; any other store URI, single-user layout, or remote store is refused (unsupported_backend). Multi-user Nix is required for the kind. (2) Variants: ALLOWLIST of verified CppNix versions (same pattern as the Docker client admission); Lix and others refused with unsupported_backend until their own verification task exists.

Host facts (2026-10-24, read-only, uid 1000 untrusted): nix --version -> 'nix (Nix) 2.34.8'; nix store info --json -> {"trusted":false,"url":"daemon","version":"2.34.8"}; nix config show: store = auto, auto-optimise-store = false, trusted-users = root, allowed-users = *, keep-derivations = true, keep-outputs = false, min-free = 0. /nix/store root:nixbld 1775, ro bind mount of the root ext4 (same st_dev 65025 as /nix/var/nix/db). /nix/var/nix/daemon-socket/socket root 0666 socket; every ancestor root 0755. Clients resolve to /nix/store/ima4p8pl9chccjprd6rdg9pqbr1qmfnq-nix-2.34.8/bin/{nix,nix-store} (multi-call). nix-daemon socket-activated (systemd). /nix/var/nix/profiles: system -> system-177-link -> /nix/store/...-nixos-system-...; default -> per-user/root/profile. gcroots/profiles is a dangling link into /tmp/calamares-root-*. gcroots/auto: 338 entries. /nix/store: 131,168 entries; /nix/store/.links: 1,609,768 entries (store was optimised at some point although auto-optimise-store is now false); 'find /nix/store/.links -maxdepth 1 -links 1' (read-only lstat of every link, what the GC's final phase does) took 4.59 s warm and found 0 unused links. Pinned-fd check (python, O_PATH|O_NOFOLLOW on the socket, passed to the child, env {NIX_USER_CONF_FILES=, HOME}): 'nix store info --json --store unix:///proc/self/fd/3' exit 0 -> {"trusted":false,"url":"unix:///proc/self/fd/3","version":"2.34.8"}, plus the daemon warning 'ignoring the client-specified setting store ... not a trusted user'; with NIX_REMOTE=local?root=/nonexistent and NIX_CONFIG='store = local?root=...' added it still bound the pinned daemon (exit 0); without --store the same env exits 1 'creating directory /nonexistent-diskgc2/nix/store: Permission denied'. NOT run: any GC, and 'nix-store --gc --print-roots' / '--query --roots' (findRoots unlinks dangling gcroots/auto registrations, TASK-98). Nix 2.34.8 source read (gc.cc, local-store.cc, unix/file-system.cc, daemon.cc): GC stops after the path that makes bytesFreed > maxFreed; maxFreed 0 deletes nothing; deletion order is readdir order of the store; bytesFreed counts st_size of files with nlink 1 or 2 (2 assumed to be .links), 0 for nlink >= 3; the unused-links phase runs after the deletion loop even when the limit stopped it; each dead path is invalidated before its files are deleted; the daemon's MonitorFdHup interrupts the worker when the client hangs up; CollectGarbage refuses ignoreLiveness for every client.

Design written on branch task118-design (docs/safety.md nix-store, reference.md Backend skips and errors + Backend version allowlist + final nix-store profile shape, architecture.md Nix unsized, testing.md nix-store backend vectors + Nix GC container scenarios; JSON skip enum: unsupported_store removed, backend_unavailable added, with the two test lists that pin it).
Reviews. codex round 1 (cross-model, adversarial, read Nix 2.34.8 source): BLOCKERS: root check passable via per-link indirection; realStoreDir unbound; root check and GC on separate connections; NIX_DAEMON_SOCKET_PATH uncaught; second root DB and gcroots/auto unlinking vs the PRD must-never; order and trusted-settings wording. mped-architect round 1: FIX FIRST: step 1 rejected every real CppNix (bin/nix-store is a symlink to the multi-call nix, verified on host); nix argv lacked --extra-experimental-features nix-command; caller-env 'nix store info' would connect to a user-selected remote store (replaced by 'nix config show store', verified to open no store and to reflect NIX_CONFIG/NIX_REMOTE); a bare connect misses a failed socket-activated daemon (now a bounded WORKER_MAGIC handshake probe); unstated canary cost and step 8->9 window. Changes: all folded in; step 8 now checks every generation link (bounded) and states the indirection false pass, realStoreDir and the 8->9 window as limits; allowlist split into daemon and client lists. codex round 2: handshake (authPeer before WORKER_MAGIC_2) and root-check claims confirmed against source; one new blocker: NIX_STATE_DIR (and NIX_STORE_DIR/NIX_STORE) redirect 'auto' without showing in the store setting (resolveStoreConfig takes a local store when the state dir is writable) -> step 5 now refuses non-default NIX_STATE_DIR, NIX_STORE_DIR, NIX_STORE, NIX_DAEMON_SOCKET_PATH (env names confirmed in 2.34.8 globals.cc/store-api.cc). mped-architect round 2: GO (non-blocking: clarify the 1,024 bound -> done; probe may log a daemon error per preflight -> TASK-36 to observe). Not adopted: root-DB mismatch as error instead of skip (kept under the one rule).
Open for Mark: (a) PRD 'A Nix GC root, profile or generation removed' must-never vs the stated limit (second root database over the same physical store) and Nix's unlinking of dangling gcroots/auto registrations during root discovery: proposed PRD wording 'a GC root, profile or generation of the bound daemon's root database'; (b) remote Nix stores reported as remote_backend (consistent with Docker and architecture.md) instead of unsupported_backend as in the 2026-10-23 ruling.
<!-- SECTION:NOTES:END -->
