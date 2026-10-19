---
id: TASK-18
title: 'fs kind: generic delete action with flock, live recheck and exclusions'
status: Done
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-19 22:25'
labels:
  - wave-2
  - irreversible
  - safety
dependencies:
  - TASK-16
references:
  - docs/safety.md#filesystem-candidates
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: most cruft (/tmp leftovers, caches) is plain files. Wire the fs kind's delete action through the engine and in-use checks: flock the candidate, eligibility walk and in-use under the lock, identity-checked delete; PrivateTmp entries and network filesystems excluded; tmpfs matching behaviour.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The 100k-entry /tmp fixture: every eligible pytest-of-* and nix-shell rc dir is deleted, nothing else (deletion oracle)
- [x] #2 A file created mid-delete yields an ENOTEMPTY error for that candidate, never a silent retry
- [x] #3 systemd-private-* entries are never candidates
- [x] #4 Tests drive the engine with user-profile fixtures (no built-in names before wave 4)
- [x] #5 A candidate on a network filesystem is never deleted (model test); a socket or FIFO is never deleted (container test)
- [x] #6 A dead FUSE mount in /tmp does not hang the scan (container test with /dev/fuse and a FUSE server that never answers)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-5 (deletion engine): the fs kind wires engine::walk -> (in-use, task-6) -> engine::delete under the candidate's flock. Not in the engine, still this task's: mountinfo first (a dead FUSE mount point inside a candidate would block the walk's statx; the walk itself only detects mounts by stx_mnt_id/EXDEV), systemd-private-*, network filesystems, .diskgc-keep in directories between the profile root and the candidate (engine::KEEP_MARKER; the walk only looks inside), the idle filter from WalkRecord::newest_change() (None = some time unreported: not idle), protect/negation. The flock fd should be checked against WalkRecord::root().identity.id. make_writable maps to DeleteOptions::make_writable. A file created mid-delete gives End::Failed with ENOTEMPTY (AC #2 here). Sockets/FIFOs are refused by the walk (contains_ipc; container test engine_skips_sockets_and_fifos exists).

From task-68 (read-only spike): crates/diskgc-core/src/fs_kind/plan.rs is the read-only half of this task's fs kind: segment-wise discovery (mountinfo first: a mount point is never statted; systemd-private-* never a candidate nor entered; outermost root first; identity dedupe), then per candidate the source's has/has_not, other_fs, keep markers between root and candidate, the engine walk, identity, idle (WalkRecord::newest_change) and the in-use check. It judges each candidate while its parent is open (one fd per pattern level), which a reclaim cannot reuse as is (it needs flock + walk + check + delete under the lock, with a scan <= 1 s old). executor::ensure refuses a reclaiming run over kind fs as a usage error before anything else (model test a_reclaiming_ensure_over_fs_candidates_is_refused_before_anything): this task lifts that refusal. Measured on the laptop: at nice 19 / ionice idle under load 10 to 90, ~55 candidates/s judged; nix-shell-rc (105k candidates, ~3 KiB each) did not finish a 20 min dry run; contains_ipc kept 158 pytest dirs and 199 nix-shell.* dirs (stale test FIFOs and sockets under the default policy). The fs size estimate counts hard links per name (claude-code-tmp: 2,794 MiB vs du 2,599 MiB); the cargo kind dedupes by identity, the fs kind should too. See task-70 for scan cost.

From task-68 final review (commit 19a47e3): (1) Codex F4 residual: fs_kind/plan.rs Walk::visit can still finish past the deadline with exit 0 when the LAST matching entry's statx crosses it and its type fails find.type, when a listing returns TooMany or a skippable error (bypasses the post-listing check), and between has and has_not in source_where::holds. Check the deadline before predicate I/O and once before discovery reports success; add tests for each exit. (2) mped N1: Walk::candidate repeats the identity/seen-set/duplicate logic in the new mount block; do identity+dedup once at the top, then mounts, then where, then judgement, when splitting discover from judge. (3) mped N4: Planned.private_tmp doc says it counts names a pattern matched; it now counts roots too. (4) QA: the PrivateTmp root check (F3) is by path name only; a bind mount of a systemd-private-* dir elsewhere is not caught (symlinked roots are already refused). State it in safety.md or check by identity.

From task-71: engine::walk refuses VCS checkouts (.git/.hg/.svn anywhere: contains_checkout) for every plain delete. The fs delete path gets this by calling engine::walk; never route fs candidates through engine::walk_cargo_profile_entry (the Cargo kind's exemption, crate-private).

From task-71 r2: the path-component checkout rule (a .git/.hg/.svn name on the path between the profile root and the candidate -> contains_checkout, Mark 2026-10-18) lives only in the fs planner's discovery (fs_kind/plan.rs carries it down like keep markers). The delete path must get candidates from that discovery, or re-apply the rule; add a test that a reclaiming fs run never deletes proj/.git/objects found by a broad pattern.

From task-80 (supersedes the two task-71 notes above): engine::walk_cargo_profile_entry is gone. The checkout guard is the source profile's checkouts setting (refuse by default, allow only where a profile says so), carried in engine::Rules::checkouts; build the walk rules from the tier (executor's rules(tier, owners, log): Rules { checkouts: tier.checkouts, ..Rules::new(owners) }) and engine::walk applies it. The path rule is one pure call, engine::checkout_metadata_below(rules.checkouts, root, at) -> Option<Skip> (fail closed: a candidate not below its root is refused); the fs planner calls it in Walk::judge, and the delete path re-applies it with the same call if its candidates do not come from that discovery. Keep the test that a reclaiming fs run never deletes proj/.git/objects (profile without checkouts) and add its allow twin.

Implemented (task-18 implementer, 2026-10-19), staged for the orchestrator's deep gate, not committed.

Protocol (docs/safety.md, Deletion, its single home): fs_kind::discover (mountinfo first; root refused by its mountinfo line: network fs, PrivateTmp by path or by the mount's fs_root; identity+dedupe, then mounts, then where) -> fs_kind::tally (place, reach, keep markers, where, walk; no lock/idle/in-use) -> per candidate fs_kind::reclaim: deadline + fresh mountinfo + Judge::place (checkout path rule, other_fs, mount at/inside) -> reach (root again, chain of dirs held, identity vs discovery via same_object) -> no_keep_markers -> inuse::lock_candidate (flock try; locked; locked dir must be the object found) -> under the lock: fresh mountinfo + place, no_keep_markers on the held dirs, Judge::source_where (changed), Judge::walk (engine::walk with the tier's Rules incl. checkouts; root identity vs found; idle via newest_change, None = never idle), inuse::locked_is_walked, Checker::check (freshness rules; a blocked scan is dropped first) -> inuse::delete_cleared (DeleteOptions make_writable, deadline) -> release. ENOTEMPTY -> ReclaimEnd::Failed -> an error of that candidate (and an action if part-way), never retried. The dry run (fs_kind::plan) is the same discovery and judgement plus a scan reused per DRY_RUN_SCAN_REUSE.

Executor: Tier.action replaces the operation string; ensure's allowlist admits (CargoTarget, CargoLockProtocol) and (Fs, Delete) only (a command action stays a usage error, task-37); Reclaim::run matches the kind exhaustively into walk_cargo / walk_fs; one Reclaim::record maps every ReclaimEnd (both kinds) to actions/skips/errors with statvfs before and after (null when unmeasured). Stop, Judged, ReclaimEnd and judged! moved to crate::outcome (cargo_target re-exports them). resolve::resolve_in resolves against a given mount table.

Task-68 residuals: deadline checked after every listing whatever it returned, before each where predicate, and before discovery reports success (tests: fs_dry_run::discovery_stops_at_the_deadline_through_every_exit); mped N1 order (identity+dedupe, mounts, where); Planned.private_tmp doc; QA PrivateTmp bind mount: checked by the mount's fs_root (unit test a_root_is_judged_by_its_path_and_its_mounts_line_first) and the copy/rename limit stated. The fs size estimate counts each object once (fs_dry_run::the_size_estimate_counts_a_hard_linked_object_once).

Tests: crates/diskgc-model/tests/fs_reclaim.rs (18), fs_dry_run.rs (+2), fs_kind unit tests (+2), cli.rs gate test extended to an fs reclaim; diskgc-e2e/src/fs_reclaim_suite.rs: fs_reclaim_tmp_fixture_deletes_every_eligible_candidate_and_nothing_else (fast, ~12k entries, 2,000 candidates), fs_reclaim_100k_entry_tmp_deletes_every_eligible_candidate_and_nothing_else (full), fs_reclaim_never_deletes_a_socket_or_fifo (fast), fs_reclaim_a_dead_fuse_mount_does_not_hang_the_scan (fast; the runner passes /dev/fuse when the host has it).

Open limit (not fixed, follow-up suggested): the in-use scan statx's /proc/<pid>/{fd/*,cwd,root}; a visible process with a file open or its cwd on a dead FUSE/network mount hangs the scan. Likely fix: AT_STATX_DONT_SYNC in diskgc-linux's link_target, with a container test that needs a FUSE server that answers then stops.

Round 2 (task-18 implementer, 2026-10-19), staged, not committed. Supersedes the round-1 note's details where they differ.

B1 (Codex): engine::may_be_networked (was is_network_fs) fails closed on FUSE: every fuse, fuseblk and fuse.<name> is refused (unsupported_fs) unless <name> is in LOCAL_FUSE_TYPES (fuse-overlayfs only, justified in the doc comment and safety.md); NETWORK_FS_TYPES names network, cluster and VM-shared types and FUSE network clients (ceph-fuse, sshfs, rclone, s3fs, gcsfuse, glusterfs, lizardfs, mfs/moosefs, juicefs, ...) so the reason is clear. Applies to both kinds (Cargo discovery uses it too).

S1 (Codex): the deadline is read right before each unlinkat (after the identity statx, in engine::delete's checked_unlink) and each fchmod (make_writable). ModelHost's unlink check allows clock reads between the statx and the unlinkat.

S4 (mped S3): fs_kind::judge has Judge::approach (place, reach, keep markers), Judge::again (place and keep markers under the lock) and Judge::contents (where and walk, idle flag); plan, tally and reclaim call these. Reached::candidate and one Tally type (plan's Eligible merged into it). Judge steps are pub(crate).

S5 (mped S4): inuse::delete_unused is gone from production code; the composition lives in diskgc_model::support (test-only), used by the model tests and the e2e driver (diskgc-e2e now depends on diskgc-model normally). The shared tail of every deletion is inuse::clear_and_delete (check against a fresh scan, never a blocking one, the caller's own check on that scan, delete_cleared, one retry of an expired clear), used by the fs and Cargo protocols; Checker::serves is the one options check; ReclaimEnd::of maps the engine's End for both kinds; From<Stop>/From<Skip> for ReclaimEnd.

S6: ModelHost has a mount event; fs_reclaim::a_mount_made_after_the_flock_is_contains_mount_and_never_touched.
S7: fs_reclaim::a_candidate_replaced_before_its_reach_is_never_opened_or_locked.
S8: fs_reclaim::the_deadline_between_candidates_stops_before_the_next_one_is_touched.
S1 test: fs_reclaim::the_deadline_is_read_right_before_each_unlink_and_fchmod.
B1 test: fs_reclaim::an_unknown_fuse_filesystem_is_refused_and_a_local_one_admitted (and fuse.ceph-fuse, virtiofs in a_network_filesystem_is_never_reclaimed_or_touched).
S9: the /tmp fixture sleeps idle_for + 1 s after the last eligible entry is made.
NITs: a PrivateTmp root is unsafe_path (a misconfiguration: an error excluding the tier) in discovery and reach alike; the FUSE e2e test is fs_reclaim_a_dead_fuse_mount_is_never_touched and only it gets --device /dev/fuse (suite::FUSE_TESTS).

Docs: safety.md Before the walk (mounts first covers discovery and judgement only; network filesystems, fail closed on FUSE, the allowlist; PrivateTmp root as unsafe_path), In use (a process using a dead mount hangs the scan, both kinds; AT_STATX_DONT_SYNC fixes FUSE only, NFS stays a limit through d_weak_revalidate; task-102), Deletion (tally includes where; the deadline right before unlinkat and fchmod; which opens the deadline covers; the mount race after either table read), Deadlines (pointers). testing.md: the profile DoD rule split into every-profile and built-in-only, bites named in task notes; model mount event; unlink check wording; FUSE_TESTS. reference.md: Built-in profile library points to the DoD rule.

Bites (scratch copy, own target dir, restored by copy): each mutation and the test that went red is listed in the round-2 report to the orchestrator.

Round 3 (Codex continuation, 2026-10-19): supersedes round-2 B1 allowlist. All FUSE types, including fuse.fuse-overlayfs and fuseblk.<subtype>, are refused because mount type cannot establish backing-storage locality. Cargo discover and reach now preflight the mountinfo entry before opening/statting the root. make_writable prepares and identity-checks a readable directory fd before its final deadline read; LinuxHost::fchmod now performs only the gated fchmod on that prepared fd. fs and Cargo model tests cover FUSE refusal and preflight. Task-102 was narrowed to FUSE via backlog CLI; dead NFS remains a separate documented limit. docs/reference.md points to the single profile DoD. just check passed; container e2e is in progress.

Round-3 verification update: nix develop -c just e2e-one engine_make_writable_deletes_a_read_only_tree passed on the Nix container image; LinuxHost fchmod works with the prepared readable fd. Full just e2e and review gate remain for the orchestrator.

Bite proof from saved scratchpad logs (mutate in scratch, run, restore; exact mutation -> red test): let-private-tmp-through -> fs_reclaim::systemd_private_entries_are_never_candidates; ignore-network-fs -> fs_reclaim::a_network_filesystem_is_never_reclaimed_or_touched; ignore-mounts-inside and statx-mount-points -> fs_reclaim::a_mount_at_or_inside_a_candidate_is_never_touched; delete-a-fifo -> fs_reclaim::an_fs_reclaim_deletes_every_eligible_candidate_oldest_first_and_nothing_else; drop-enotempty-surfacing -> fs_reclaim::a_file_created_mid_delete_is_an_enotempty_error_never_retried; route-fs-into-cargo-reclaim -> fs_reclaim::each_kind_reaches_only_its_own_protocol; skip-rejudge-under-lock -> fs_reclaim::every_check_is_made_again_under_the_candidates_lock; reuse-stale-scan -> fs_reclaim::a_process_that_opens_a_candidate_after_an_earlier_scan_keeps_it; reuse-blocking-scan -> fs_reclaim::a_blocking_scan_is_not_reused_for_the_next_candidate; drop-identity-recheck -> fs_reclaim::a_candidate_replaced_after_discovery_is_changed_and_nothing_is_deleted (round 2); count-hard-links-per-name -> fs_dry_run::the_size_estimate_counts_a_hard_linked_object_once; drop-final-discovery-deadline and drop-post-listing-deadline -> fs_dry_run::discovery_stops_at_the_deadline_through_every_exit (round 2); S1-no-deadline-before-unlink and S1-no-deadline-before-fchmod -> fs_reclaim::the_deadline_is_read_right_before_each_unlink_and_fchmod (round-2 saved logs). Limitation: drop-where-deadline alone stayed green; only drop-where-deadline-and-backups turned that deadline test red. Saved records: scratchpad/bites-round1.txt, bites-round2.txt, bites-final.txt and bite2-*.log in the Claude session directory.

Round 4 (Codex continuation, 2026-10-19): shared engine::walk refuses a systemd-private-* candidate or descendant by name before statx, so a broad fs candidate containing PrivateTmp is kept (contains_special). Cargo root discovery/reach preflight both path components and mountinfo fs_root for PrivateTmp, and discovery skips nested PrivateTmp before opening it. Cargo discovery and target/profile inspection preflight mount points from mountinfo before statx, including a nested dead FUSE mount; the mount table remains a snapshot, with the documented later-mount race. Model regressions: fs_reclaim::a_plain_delete_candidate_containing_private_tmp_is_kept, cargo_target::cargo_discovery_never_enters_private_tmp_below_a_root, cargo_target::cargo_discovery_does_not_stat_a_mount_below_a_root_or_profile, and a unit check for a bind-mounted PrivateTmp fs_root. Container regression fs_reclaim_never_enters_private_tmp_inside_a_candidate passed via nix develop -c just e2e-one. Full fixture corrected from 99,000 to 100,001 modeled entries plus fixture overhead; full-scale container rerun and full just e2e remain for orchestrator. nix develop -c just check passed; git diff --check passed.

Round 5 (Codex final architecture audit, 2026-10-19): Cargo reach now checks mountinfo before statx of the project Cargo.toml, both when the target lies under its root and when the target is the root (parent has a fresh mount snapshot). A mounted manifest is treated as unknown, hence detached. Shared engine::walk now reads mountinfo before statx of the candidate and checks every listed descendant name against mount points before statx; this closes Cargo profile-entry nested dead-FUSE hangs in eligibility/tally and again under the reclaim locks. The mount table remains a snapshot, so a mount made afterward remains the documented race. Model regressions cargo_reclaim::a_mounted_manifest_is_never_statted_during_eligibility_or_reclaim (both root shapes) and cargo_reclaim::a_nested_mount_in_a_profile_entry_is_never_statted assert no trace at either mounted path through eligibility and reclaim. Existing engine mounted-candidate test now asserts no statx/open/read_dir. nix develop -c just check passed (fmt, clippy, workspace tests); cargo_reclaim test suite passed; git diff --check passed. Scratch-copy mutation bites (source restored; cmp matched live tree): removing nested PrivateTmp guard in engine::walk made fs_reclaim::a_plain_delete_candidate_containing_private_tmp_is_kept RED, with /tmp/old and its PrivateTmp subtree deleted; removing engine::walk descendant mount preflight made cargo_reclaim::a_nested_mount_in_a_profile_entry_is_never_statted RED, trace showed Statx at /tmp/w/p/target/debug/build/dead; removing has_manifest mount preflight made cargo_reclaim::a_mounted_manifest_is_never_statted_during_eligibility_or_reclaim RED, trace showed mounted Cargo.toml touched. Full container e2e and reviews remain for orchestrator.

2026-10-20 E2E harness scale follow-up: Nix just e2e-full passed 154/154, but the 100k fixture oracle judgement took 506 s because Fixture::covers linearly scanned roughly 16,000 eligible prefixes for each snapshot/trace path. Debian e2e was stopped at 42 passed, 0 failed to avoid overrunning the 900 s budget; it did not fail a test. diskgc-e2e/src/oracle.rs now builds a borrowed HashSet of eligible, concurrent-writer and DISKGC_STATE_DIR roots once per Oracle::judge_runs, then probes path ancestors; public diff/traced_violations build a fresh local index per call, and Fixture::covers retains its public behavior. Empty prefix keeps Path::starts_with semantics. Deterministic unit test with 16,000 eligible roots plus state allowlist observes 2 ancestor probes for a covered deep path and 4 for an uncovered path, independent of root count; semantic tests compare the index to the original for ancestor/sibling, noncanonical and state paths, and a mutation of public fixture fields. Verification: nix develop -c cargo test --manifest-path diskgc-e2e/Cargo.toml oracle::tests passed 11/11 (9.27 s warm wall time); nix develop -c cargo clippy --manifest-path diskgc-e2e/Cargo.toml --all-targets --locked -- --deny warnings passed (6.43 s); nix develop -c just check passed (21.80 s); git diff --check passed. Full E2E performance after this change remains to be measured by QA; no full E2E was run during this edit.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Final gate on optimized source SHA256 600c01adc8ece88cacf52068b456d3e7c6251d2083690e7d9c08ff08b142807a: nix develop -c just e2e exit 0 (Nix fast 88/88); nix develop -c just e2e-full exit 0 (Nix full 154/154, Debian full 93/93); workspace 563 pass/1 ignored; corrected 100001-entry oracle PASS in both full images; dead FUSE actually ran and passed in all container suites. QA GO; MPED architecture GO. Known dead-FUSE /proc scan follow-up is TASK-102.
<!-- SECTION:FINAL_SUMMARY:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
