---
id: TASK-169
title: >-
  In-use and mount blind spots not in docs/safety.md: io_uring/loop/overlay
  references, mounts in other namespaces, hidepid, non-initial pid namespace
status: To Do
assignee: []
created_date: '2026-10-25 01:14'
labels:
  - hardening
  - safety
dependencies: []
references:
  - docs/safety.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-13 async QA (b009e59, read-only reasoning; none reproduced). Each is either a hole for user fs profiles or an undocumented limit; 0.1 built-ins are less exposed (pytest-tmp kept by its lock/idle rows, nix-shell-rc producer_unknown).
1. Kernel-held file references with no /proc fd/map entry: io_uring IORING_REGISTER_FILES then close(fd); loop device backed by a candidate file (udisksctl loop-setup, no root); overlay upper/work/lower dirs of a live overlay whose merged mount is outside the candidate (in-use matches by dev/ino; processes see the overlay's dev/ino; mountinfo upperdir= is never parsed). PRD: "a filesystem candidate deleted while a visible process has one of its files open" - with io_uring the process still uses the file.
2. A directory that is a mount point only in ANOTHER mount namespace (rootless podman overlay merged/, unshare -rm sessions): since Linux 3.18 removing it succeeds and lazily detaches that mount; diskgc sees an empty user dir.
3. Cargo across overlay: a Cargo in a container locks the overlay inode; host diskgc reaching .../overlay/<id>/diff locks the upper inode: the flocks do not conflict.
4. hidepid=invisible hides the user's own newgrp/sg processes from the plain in-use scan (only Owner PID checks hidepid via PidTable::process_list_complete); the model's proc_hidepid does not hide anything from pids().
5. diskgc in a non-initial pid namespace sharing /tmp: host processes are invisible; pytest row 7 still permits old sessions; Visibility section silent.
6. Lock exclusion assumes one kernel (NFS/Samba/virtiofs/9p exports of a project dir): state as an operator assumption under cargo-target.
7. btrfs nested subvolume inside a candidate: walk compares mount ids, not devices; the subvolume is walked and (4.18+) its empty root removable by the owner. Only judge other_fs (TASK-129 M17) sees a different device.
To confirm 1-3: container tests (io_uring register+close, then ensure --dry-run: eligible?; unshare -rm tmpfs mount at /tmp/c/m with a cwd inside; flock -n on merged vs upper). 4 needs a VM.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each item reproduced in a container/VM or argued unreachable, evidence recorded
- [ ] #2 Reproduced items fixed or documented in docs/safety.md (In use, Visibility, cargo-target) as limits
<!-- AC:END -->
