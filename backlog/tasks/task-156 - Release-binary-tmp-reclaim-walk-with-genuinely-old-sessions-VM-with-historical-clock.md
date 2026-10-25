---
id: TASK-156
title: >-
  Release-binary /tmp reclaim walk with genuinely old sessions (VM with
  historical clock)
status: Done
assignee: []
created_date: '2026-10-24 23:17'
updated_date: '2026-10-25 11:49'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-65: no container test runs the release binary through a real /tmp reclaim past the 1h floor, because the release binary has no test clock offset and rootless containers cannot backdate ctime. Use a VM with a historical clock (like vm-historical-ctime) or a test that waits; relates to TASK-154.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
t156 (5a535b7): flake check vm-release-tmp-reclaim, in just e2e and e2e-full. NixOS guest installs release-01 package (PATH, tmpfiles.d drop-in, example config), tmpfs /tmp; guest clock 5 d back while pinned pytest 9.1.1 / Nix 2.34.9 make per user (alice, bob): pytest-0/1 SIGKILLed (dead-pid .lock), 2-4 ended, 5 live; dead legacy nix-shell + exited nix develop rc dirs; live of each; decoys. Release binary as normal user: --select profile:pytest-tmp / profile:nix-shell-rc (JSON), example config as shipped (exit 2 cause config) and with root fixed (human + JSON, exit 1 exhausted). Result: 0-2 reclaimed (idle_age ~432000 s), 3/4 in_use temp_root, 5 in_use owner_pid, dead rc dirs reclaimed, live nix develop owner_pid, live legacy nix-shell in_use fd 255 (rc script); idle-7d -> not_idle; exact /tmp diff; live pytest finishes exit 0. Runs: exit 0 x4 (incl. committed sha, 30 s wall, load ~2-4). Evidence: tmp/walks/2026-10-25-t156-release-tmp-reclaim/. Follow-up candidate: human/JSON detail for another user's 0700 dir leaks Rust Debug: 'open LinuxFd(OwnedFd { fd: 4 })/.: EACCES (13)'.

Merged to master as 5a535b7 on 2026-10-25T11:49Z.
<!-- SECTION:NOTES:END -->
