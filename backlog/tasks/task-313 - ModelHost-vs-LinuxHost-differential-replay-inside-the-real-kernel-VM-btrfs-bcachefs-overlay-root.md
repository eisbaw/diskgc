---
id: TASK-313
title: >-
  ModelHost-vs-LinuxHost differential replay inside the real-kernel VM (btrfs,
  bcachefs, overlay, root)
status: To Do
assignee: []
created_date: '2026-11-02T11:33:00Z'
labels:
  - test
  - vm
  - model
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-02: 'ensure we tackle these findings ... we need more real-world VM tests, so we need to bake into the e2e tests that we do clean up afterwards.' From COMPASS consult #3 (master c286b3b). COMPASS: the model and reviewers share kernel beliefs; only a kernel checks them. diskgc-e2e/src/differential_suite.rs replays model vectors against LinuxHost on tmpfs only. Run it inside vm-real-kernel on btrfs (incl. subvolumes and two loop members), bcachefs, an overlay (btrfs upper / ext4 lower), and as root, so each model kernel fact (st_dev/stx_subvol per subvolume, overlay identities, loop backing, kernel-thread /proc behaviour) is checked against the real kernel. Model divergences found become bugs or documented model limits.
<!-- SECTION:DESCRIPTION:END -->
