---
id: TASK-196
title: >-
  Structure: one mount screen, one {uid} expansion, one freshness loop; split
  executor.rs
status: To Do
assignee: []
created_date: '2026-10-25 06:00'
updated_date: '2026-10-25 12:44'
labels:
  - structure
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the 10-task codebase review at 72b376c (2026-10-25). (1) Mount screening written five times with different mappings (executor.rs:preflight_roots, screen_overlap_mounts copy of screen_mount_chain, fs_kind/judge.rs:check_root, cargo_target/discover.rs:resolve_root, invocation/main); 'is a mount point' spelled four ways; drift produced TASK-178 -> one resolve::screen_root + is_mount_point (with TASK-182). (2) {uid} expanded by raw str::replace in eight places -> expand_uid. (3) Checker::check and clear_and_delete each have their own FRESH_ATTEMPTS loop -> one. (4) executor.rs 5.5k lines; ensure_inner and ensure_grouped_inner duplicate lock->measure->preflight->protect->reclaim -> single-tier as a one-group grouped ensure; split tier.rs/preflight.rs/policy.rs/plan.rs. (5) debug_assert! invariants in cargo_target/reclaim.rs:still_reached and nofile.rs:raise_to_hard -> assert!; fs_kind/liveness.rs compares dev_ino only -> Identity::of. (6) two modules named outcome; stale diskgc-linux/Cargo.toml comment; orphaned doc above fs_kind/judge.rs:keep_newest; units.rs size errors repeat the byte count.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
COMPASS round 3 (2026-10-25): item (1) absorbs TASK-182 (one mount screen); do after 0.1 and BEFORE TASK-31 adds index-driven walking to executor.rs.

2026-10-25 inuse2 ee643a6 on branch inuse2 (not merged; base master 28e9456). See commit message for the change. Item 3 (one freshness loop) done on branch inuse2: Checker::with_fresh_clear replaces the loops in Checker::check, clear_and_delete and the command action.
<!-- SECTION:NOTES:END -->
