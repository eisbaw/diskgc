---
id: TASK-93
title: Kind or command profile for Podman storage (images, build cache)
status: In Progress
assignee: []
created_date: 2026-10-19 11:48
updated_date: '2026-11-03T13:13:00Z'
labels:
  - home
dependencies:
  - TASK-37
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the home survey (2026-10-18) found ~8.7 GiB of rootless Podman images in ~/.local/share/containers/storage, and diskgc's own e2e images live there. Rootless overlay files are owned by subuids, so a plain delete is refused (not_owner) and would corrupt storage anyway; Podman must clean itself (podman image prune [-a], podman system prune), like the Docker kinds (task-35). Decide kind vs command action, how 'unused' and age are judged (podman's own filters: until=, dangling), what it reports freed, and refusal while a podman process is running.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A profile (or kind) prunes Podman images with Podman's own command, never by deleting storage files; examples and a model test
- [ ] #2 Verified in a container or read-only on a real machine (dry run lists what podman would prune)
- [ ] #3 Docs say how it relates to the Docker kinds and to rootful /var/lib/containers
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
TASK37 command actions landed in adcbc3f, but a generic fs command profile rooted at ~/.local/share/containers/storage is not viable: read-only dry-run on this host found one storage candidate skipped not_owner because rootless overlay includes uid 101000. Design Podman-owned discovery/cleanup protocol or a command profile whose candidate is an owned stable directory and whose preflight/safety rules do not walk subuid overlay contents. Require container test with a held file and real read-only pruning evidence before shipping an example.

2026-10-20 isolated Podman 5.8.7 protocol probe: CONTAINERS_STORAGE_CONF=/dev/null with explicit --remote=false, --root, --runroot and --storage-driver overlay reported the fresh temporary store. Two local FROM scratch builds produced one dangling image; image list --all --filter dangling=true --format json listed exactly that ID; image prune --force in that isolated store removed it and retained the tagged image. A read-only pinned query of the real rootless graphRoot returned the same five image IDs as ambient Podman and zero dangling; no host prune was run. However podman info graphOptions was {} even when --imagestore pointed to a separate directory, so it does not expose that store. The CLI --root is a pathname Podman reopens; a check of graphRoot identity before prune cannot bind later writes to that checked directory. Passing a held directory fd as --root /proc/self/fd/3 failed on Podman 5.8.7 with exit 125: overlay: failed to make mount private: mount /proc/self/fd/3/overlay:/proc/self/fd/3/overlay, flags: 0x1000: invalid argument. Under the present fd-based safety contract, this blocks a Podman pruning kind; do not ship an fs command profile. A future design must bind the writer to the checked store or explicitly establish and test an acceptable race policy. TASK93 stays open.

Further read-only host evidence: podman image prune --dry-run --help exits 125 (unknown flag). podman system df --format json reported 6,713,465,194 RawReclaimable image bytes while dangling=true listed none. Podman docs warn shared layers can make that estimate exceed bytes actually freed. The five local Nix E2E images report CreatedAt 1970-01-01 despite recent loads, so --filter until= would use creation time rather than last use and cannot safely guard --all. The isolated probe directory is /tmp/diskgc-task93-probe.umBX6p; no host image or store was pruned.

2026-11-03: faster profile rhythm; in progress in tmp/worktrees/t93.
<!-- SECTION:NOTES:END -->
