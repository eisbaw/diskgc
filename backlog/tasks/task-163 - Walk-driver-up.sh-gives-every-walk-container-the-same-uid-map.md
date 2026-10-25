---
id: TASK-163
title: 'Walk driver: up.sh gives every walk container the same uid map'
status: Done
assignee: []
created_date: '2026-10-25 00:44'
updated_date: '2026-10-25 03:52'
labels:
  - followup
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Gotcha 2026-10-25 (TASK-65 walk) says walk containers must map tester to distinct host uids, but tools/walk/up.sh has no --uidmap option; TASK-146..152 walk started walk-diskgc-docs by hand with --uidmap 0:0:1 --uidmap 1:2:65534 (and gidmap). Add an env/arg to up.sh (e.g. WALK_UID_SHIFT) so a parallel walker gets its own range reproducibly.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented on branch walk-tools 4e707b8 (unmerged). Evidence tmp/walks/2026-10-25-walk-tools/. Gate: nix develop -c just check exit 0 (1116 passed, 0 failed, 1 ignored; unittest 34 OK). just e2e not run. just walk-up NAME: own container, tmux session walk-diskgc-NAME, uid slot via tools/walk/uid-slot.sh (2048-id slots of the podman unshare range, label diskgc.walk.uid-slot; podman = slot 0 = default host uids). wt-a slot 28 / wt-b slot 31 concurrently: both ensure runs reclaimed 6 targets, rc 0 (05-*; first attempt wt-b kept all as in_use_unknown, host too busy, load 37, 03-*). Uid gotcha holds only with --pid=host: reproduced (06-*, same slot 0 reclaimed / 8 in_use; distinct slot 8 reclaimed). WALK_PID_HOST=1 added; walk-install NAME BIN TAG streams a binary in as diskgc-TAG.

Merged to master as aaa6c36 on 2026-10-25T03:52Z.
<!-- SECTION:NOTES:END -->
