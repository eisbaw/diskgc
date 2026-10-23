---
id: TASK-125
title: >-
  containers-image-tmp: verify real Podman/Buildah spool reuse lifecycle before
  re-admission
status: To Do
assignee: []
created_date: '2026-10-23 17:11'
labels:
  - profiles
  - verification
dependencies:
  - TASK-112
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: Mark (2026-10-23, TASK-112 D2) withdrew containers-image-tmp from the built-in library: its 3-day floor cannot prove a producer is done with a /var/tmp spool, and the VM test vm_containers_image_dormant_spool_exposes_liveness_gap shows a fixture producer that closes a spool and reuses it on resume is judged eligible. Whether real containers/image (copy to containers-storage: storageImageDestination temp dir, PutBlob then Commit) and Buildah (/var/tmp/buildah* build/commit staging) ever leave a spool closed and reopen it later is unverified. Verify in a disposable container with the pinned Podman/Buildah: pause a pull/commit/build (SIGSTOP) at the points between write and reuse, record open fds, which paths are reopened by name, how long the gap can be, what remains after normal exit and after SIGKILL. Then either design a contract (class and residual, docs/safety.md Producer liveness) and re-admit under a new profile name, or keep it withdrawn with the evidence recorded in docs/reference.md Liveness contracts. No host deletion.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Container test pins the real producer's spool lifecycle (fds held, reuse by path, leftovers) for the pinned Podman and Buildah
- [ ] #2 Liveness contracts row updated: re-admitted with a contract class and vectors under a new name, or withdrawal confirmed with the evidence
<!-- AC:END -->
