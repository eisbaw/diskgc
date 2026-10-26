---
id: TASK-196
title: >-
  Structure: one mount screen, one {uid} expansion, one freshness loop; split
  executor.rs
status: To Do
assignee: []
created_date: '2026-10-25 06:00'
updated_date: '2026-10-26 06:38'
labels:
  - structure
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the 10-task codebase review at 72b376c (2026-10-25). (1) Mount screening written five times with different mappings (executor.rs:preflight_roots, screen_overlap_mounts copy of screen_mount_chain, fs_kind/judge.rs:check_root, cargo_target/discover.rs:resolve_root, invocation/main); 'is a mount point' spelled four ways; drift produced TASK-178 -> one resolve::screen_root + is_mount_point (with TASK-182). (2) {uid} expanded by raw str::replace in eight places -> expand_uid. (3) Checker::check and clear_and_delete each have their own FRESH_ATTEMPTS loop -> one. (4) executor.rs 5.5k lines; ensure_inner and ensure_grouped_inner duplicate lock->measure->preflight->protect->reclaim -> single-tier as a one-group grouped ensure; split tier.rs/preflight.rs/policy.rs/plan.rs. (5) debug_assert! invariants in cargo_target/reclaim.rs:still_reached and nofile.rs:raise_to_hard -> assert!; fs_kind/liveness.rs compares dev_ino only -> Identity::of. (6) two modules named outcome; stale diskgc-linux/Cargo.toml comment; orphaned doc above fs_kind/judge.rs:keep_newest; units.rs size errors repeat the byte count.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
COMPASS round 3 (2026-10-25): item (1) absorbs TASK-182 (one mount screen); do after 0.1 and BEFORE TASK-31 adds index-driven walking to executor.rs.

2026-10-25 inuse2 ee643a6 on branch inuse2 (not merged; base master 28e9456). See commit message for the change. Item 3 (one freshness loop) done on branch inuse2: Checker::with_fresh_clear replaces the loops in Checker::check, clear_and_delete and the command action.

COMPASS round 4 (2026-10-25): items 1 (one mount screen inside resolve_in) and the executor split are raised: after the 0.1 cut, before TASK-31.

2026-10-26 t182 (branch t182: 10eb58f, 11c8a8d, 792bd53; not merged; base master 0f4a2ec). Item 1 done (one mount screen + is_mount_point, see TASK-182). Item 4: split done as a pure move (executor/tier.rs, preflight.rs, policy.rs, plan.rs; Reclaim and ensure* stay in executor.rs). Single-tier as one group NOT done: routing ensure through ensure_grouped_inner failed 8 tests; differences listed in 11c8a8d (preflight before first sample vs after lock; extra statvfs per grouped tier; single-tier-only log lines; protect vs root preflight order; usage message wording; NotesOncePerRun; Docker binding time). Needs a decision on which behaviour is the contract before unifying. Mutation scope follow-up TASK-240.

2026-10-26 merged 41755fd: item 1 done (one mount screen inside resolve_in, with TASK-182); item 4 split done (tier/preflight/policy/plan, pure move). Remaining: item 2 (10 raw replace("{uid}") sites outside invocation::expand_uid), item 3 (verify one freshness loop), item 4 unification (single-tier as one-group grouped ensure changes 8 tests' observable behaviour: executable preflight order vs first sample/met exit, extra per-tier sample, 3 log lines, protect/root check order, unsupported-action wording, process-scan note de-dup, Docker bind time; needs a contract decision first), item 5 (debug_assert in reclaim.rs still_reached / nofile, liveness dev_ino), item 6.
<!-- SECTION:NOTES:END -->
