---
id: TASK-97
title: >-
  Profile: podman/buildah leftovers in /var/tmp (buildah*,
  container_images_storage*)
status: Done
assignee: []
created_date: '2026-10-19 12:19'
updated_date: '2026-10-20 03:52'
labels:
  - library
dependencies:
  - TASK-18
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the largest single reclaimable item on the dev laptop: 68.5 GiB in /var/tmp/buildah* and /var/tmp/container_images_storage* (four ~20 GB files from 2026-10-15, owned by the user, no visible process holding them). Podman and buildah leave these temp files behind when a load or build is interrupted (as diskgc's own e2e image load did on 2026-10-19 under ENOSPC). Plain delete is right here (they are not storage, only spool files), gated on in-use and idle. Source: read-only gap investigation 2026-10-19 (scratchpad gap/summary.txt).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Profile with examples matches /var/tmp/buildah* and /var/tmp/container_images_storage* top-level entries only, idle-gated
- [x] #2 A file a running podman/buildah holds open is in_use (container test with a held file)
- [x] #3 A read-only dry run on this machine lists them with sizes matching du
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-20 implementation in isolated worktree: generic source min_idle_for floor (3d for containers-image-tmp), exact model test and container fixture prepared. Read-only host dry run listed four eligible /var/tmp directories totaling 73,584,001,024 estimated bytes; each exact byte estimate matched du -sxB1. No host deletion. Container E2E and integration gate pending; leave task To Do until they pass.

Focused container E2E fs_reclaim_image_temp_minimum_idle_and_open_file passed (11.39s): old free spool deleted by oracle, held old Buildah-shaped file in_use, fresh entry not_idle. Fixture holder is a generic process, not a live Podman/Buildah operation; no buildah CLI installed on host. Full integration E2E/review pending.

Final integrated nix develop -c just e2e passed 93/93 on 2026-10-20; QA and MPED reviews GO for the opt-in profile tranche. AC #2 stays open: the container E2E holds a Buildah-shaped file with a generic process, but a live Podman/Buildah operation has not been exercised. No live host deletion.

2026-10-20 real Buildah feasibility probe: pinned Nix Buildah 1.45.2 source vendor/go.podman.io/image/v5/storage/storage_dest.go creates container_images_storage* in newImageDestination and stores blobs there until Commit; /vendor/go.podman.io/image/v5/internal/tmpdir/tmpdir.go defaults big files to /var/tmp. The cached Buildah wrapper was bind-mounted read-only from /nix/store into an already-loaded disposable diskgc E2E container (no image pull/load), run as tester uid 1000 with isolated /tmp graphroot/runroot and vfs. `buildah info` exited 1 before any image operation: /etc/subuid absent and `finding newgidmap: exec: "newgidmap": executable file not found in $PATH`; no matching spool was generated. Adding Buildah and mapping helpers to the Nix image would rebuild/reload its approximately 16 GB stream, and permission behavior remains unproven. AC #2 remains open; generic held-file E2E remains the only integrated proof. No host deletion.

2026-10-20 bounded rootful Buildah follow-up: in the already-loaded disposable E2E container, pinned Buildah 1.45.2 (`--storage-driver vfs`, isolated /tmp graphroot/runroot, no pull) could `from scratch`, add a 4 MiB file, and commit after a minimal containers policy.json was supplied inside the container. While the real Buildah commit ran, `/proc/50/fd/8` pointed to `/var/tmp/container_images_storage1074345976/1`; SIGSTOP held that process and FD for one second, SIGCONT resumed, and commit exited 0. This proves a real matching spool can be held reproducibly. AC #2 remains open because no diskgc `in_use` verdict was asserted: Buildah ran as root, and diskgc root mode requires owners: users with uid >=1000 (root is rejected); normal tester mode cannot own the root spool. The tester Buildah probe remains blocked by missing newgidmap/subuid. No host deletion or new E2E image load.

2026-10-20 rootless feasibility result: existing disposable E2E container has outer uid/gid maps 0->host 1000 (1) and 1->host 100000 (65536). With pinned shadow 4.20.2 newuidmap/newgidmap copied into that container /bin as setuid root, ephemeral /etc/subuid and /etc/subgid entries tester:2000:1000, and tester-owned home/runtime directories, pinned Buildah 1.45.2 as uid 1000 (setpriv) completed `info`, `from scratch`, `add` of a 4 MiB file, and `commit` in isolated vfs storage. No host changes, image pull, or large artifact. This establishes a viable real-operation fixture for AC #2, but the rootless held fd and diskgc in_use assertion are not yet implemented or tested; AC #2 stays open.

2026-10-20 durable TASK-97 E2E fixture implemented in isolated worktree: pinned Buildah 1.45.2 and shadow mapping helpers from Nix, rootless tester with isolated vfs storage, 4 MiB offline payload, commit stopped while a tester-owned /var/tmp/container_images_storage* blob fd remains open. Cleaner dry run reports the exact spool directory as in_use, deletion Oracle is clean, then Buildah resumes and commit succeeds. Focused `nix develop -c just e2e-one containers_image_spool_held_by_buildah_is_in_use` passed 1/1 in 5.2s using already-loaded image; no host deletion. Fixture uses a 1s policy idle floor to reach the in-use scan while the shipped 3d floor remains unchanged. AC #2 remains open until patch integration and full E2E gate.

2026-10-20 integrated in commit 607d025. Full nix develop -c just e2e passed 121/121, including a rootless Buildah 1.45.2 commit stopped with a tester-owned container_images_storage spool fd open; diskgc reported that exact candidate in_use, deletion oracle clean, then Buildah resumed successfully. QA and MPED reviews GO. Shipped 3d idle rule unchanged; test uses a 1s fixture floor. No host deletion.
<!-- SECTION:NOTES:END -->
