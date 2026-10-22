---
id: TASK-64
title: 'Host primitives: real-machine hardening blocking the 0.1 release'
status: In Progress
assignee: []
created_date: '2026-10-17 23:54'
updated_date: '2026-10-22 10:48'
labels:
  - wave-1
  - hardening
  - safety
dependencies:
  - TASK-3
references:
  - docs/safety.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: 0.1 lifts the sandbox gate in the release package (PRD Scope, Releases; docs/safety.md, Pre-release sandbox gate), so diskgc will delete on real machines with dead network mounts, FIFOs, automounts, old kernels, btrfs and busy CI hosts. These task-61 items are correctness or safety, not cosmetics, or matter only once the gate is lifted on a real machine; the rest of task-61 stays there, deferred to wave 5.

Item references are task-61's: D1-D7 are the numbered items of its description; #N are the running numbers (1)-(43) in its notes; other items are named by the note heading they sit under.

A. Blocking or hanging opens and stats:
- D3: open_beneath with Access::Read can block on a FIFO or trigger device actions (O_NONBLOCK|O_NOCTTY, or Read only with Want::Directory).
- D5: stat of /proc fd links can hang on an unresponsive FUSE/NFS mount (AT_STATX_DONT_SYNC, tie into the deadline design).
- #26: LinuxHost statx without AT_NO_AUTOMOUNT mounts an autofs point inside a candidate (a dead NFS one could hang).
B. Path resolution correctness:
- D4: Host::statx(Base::Root, ...) follows symlinks before the last component.
- D6: a symlink as the last component of a mount point gives ENOTDIR -> missing; map it to unsafe_path.
C. /proc integrity:
- task-3 final commit gate: LinuxHost::new requires fstatfs(proc).f_type == PROC_SUPER_MAGIC.
- task-3 final commit gate: a real-kernel regression test for /proc replaced after startup (mount over /proc, lazy umount exposing an ancestor procfs).
D. Deletion engine permissions and deadlines:
- #25: deadline check immediately before make_writable's fchmod.
- task-5 deep gate round 1 (2): narrow Delete::fchmod to add owner write only, so the gated surface cannot remove permissions or set setuid bits.
- task-5 deep gate round 1 (3): untested races: an entry renamed out of a candidate between walk and delete (ENOENT -> changed); a bind mount over a file inside a candidate after the walk (EBUSY -> changed).
E. Delete only what was checked:
- #32: bind a Clear to its exact WalkRecord, so a re-walk or a hard-linked candidate cannot be mixed up.
- task-6 deep gate round 3 (3): rename engine::delete to delete_unchecked or narrow its visibility; make inuse::Checker::with_options non-public or guarded.
- #27: model inode reuse (a replace keeping the inode number) so type and birth-time compares are pinned at both sites; a chown test pinning the owner check at reopen.
F. Process identity:
- task-6 deep gate round 3 (1): pid reuse within a scan is detected only by 'found gone, listed again'; identify a process by pid plus start time or by a held pidfd.
- task-6 deep gate round 1 (5): docs/safety.md limit 4 must also cover pid reuse after a process was read.
- task-3 round 5 (3): tid reuse on hosts with a small pid_max (32768) and heavy churn; quantify or detect.
G. In-use scan soundness and liveness on real hosts and kernels:
- task-6 deep gate round 1 (2): never reuse an unsettled scan in inuse::Checker.
- task-3 round 9 (4): on Linux 5.8-6.8 (e.g. Debian 12, Ubuntu 22.04, RHEL 9) pidfd_poll never reports a hang-up, so a user process released during a scan is unreadable and makes every candidate in_use; on a busy CI host ensure may reclaim nothing. Evaluate pidfd_send_signal(pidfd, 0) == ESRCH as the hang-up test.
- task-3 round 10 (1): poll the pidfd with a short bounded timeout before the next round, so an exiting process becomes gone, not failed.
- task-6 deep gate round 1 (3) and #38: the 16-listing bound under make -j fork rates and heavy load ('process list still growing'); measure, and decide what ensure and the test do with an unsettled scan.
H. Host lock tests:
- #37: a_lock_conflicts_until_its_fd_is_closed failed 1/600 at -j4, unexplained; investigate before claiming fd_table_guard covers flock.
- task-7 review H10: one guarded-spawn helper for host.rs tests (kill and reap on drop, bounded wait), a candidate fix for #37.
- D7: the host_flock acquire check must assert that the release-during-wait case actually happened.
I. Roots and config validation (the schema freezes at 0.1):
- #39: static root normalisation (reject or normalise '..', '//', '.', duplicates in find.roots, tier roots and --roots) and dedupe of overlapping roots by identity at run time.
- #39 scope note: profile find.roots (task-8) and --roots (task-10) are wave-1 surfaces; tier roots arrive with task-15, which then applies the same normalisation (see the note on task-16).
- #41: idle_for: 0s makes every known-age candidate idle; reject it at load or document it.
J. Side effects and reporting on real filesystems:
- #19: LinuxHost::read_dir should open with O_NOATIME (retry without on EPERM), so listing /tmp does not defeat systemd-tmpfiles age cleanup.
- task-6 deep gate round 1 (1): held_by_deleted_files on btrfs (device of deleted files vs the requested filesystem's statx device) and bytes double-counted when several processes hold the same deleted file.
K. Gate flag test coverage (the 0.1 release task relies on it):
- task-50 review: sandbox_gate_flag_is_enabled_nowhere scans .cargo/config(.toml) only at the repository root; a crates/*/.cargo/config.toml could set the feature or rustflags unseen.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A (blocking or hanging opens and stats: D3, D5, #26): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [x] #2 B (path resolution: D4, D6): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [x] #3 C (/proc integrity: PROC_SUPER_MAGIC check, /proc-replaced real-kernel test): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [x] #4 D (deletion engine permissions and deadlines: #25, task-5 round 1 (2) and (3)): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [x] #5 E (delete only what was checked: #32, task-6 round 3 (3), #27): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [x] #6 F (process identity: pid reuse within and after a scan, tid reuse): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [ ] #7 G (in-use scan soundness and liveness: unsettled scan reuse, Linux 5.8-6.8 hang-up, bounded pidfd poll, 16-listing bound under load, #38): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [ ] #8 H (host lock tests: #37, H10, D7): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [x] #9 I (roots and config validation: #39, #41): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [x] #10 J (side effects and reporting: #19 O_NOATIME, held_by_deleted_files on btrfs): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [x] #11 K (gate flag test scans nested .cargo/config files): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-9 round 2 (review filings H3, H7):
- H3: a dormant shell that exported CARGO_TARGET_DIR after exec is invisible to the initial-environment heuristic (environ shows the environment at exec); for an attached target its later cargo is caught by the project-dir rule, for a detached one by rule 6 only while cargo runs. Accepted limit; worth a real-machine check.
- H7: a mode-000 directory (another tool's, or a test's) inside target/tmp/<x> makes cargo_target::inspect fail closed with unreadable for the whole target, indefinitely, although target/tmp is never deleted. Consider not listing directories two levels down that cannot be profile directories, or reporting it so the user can act.

From task-53's deep gate (mped H4-H6, cargo-target reclaim protocol):
- H4: measure the lock hold time (Reclaimed::locks_held, driver cargo-reclaim held_us) on a per-unit (nightly) target with many units: phase A deletes each build/<pkg>/<hash>/fingerprint as its own part, each with a lock re-listing (read_dir + statx of every profile entry), an open of its unit dir and an in-use check; measured so far only on a classic target with 100k files in deps/ (0.49 s untraced).
- H5 (optional refactor): keep the inspected ProfileDir inside ProfileWalk, so reclaim needs no parallel 'walked' vector; it needs a clone of inspected data or a lifetime on the public ProfileWalk, so not done in task-53.
- H6: a lock file hard-linked into two profile directories (same inode) makes the protocol's second flock on it fail against diskgc's own first lock, so the target is 'locked' with a message blaming 'another process'. Safe (nothing deleted) but misleading: detect duplicate lock identities across the target (e.g. in judge_layout -> unsupported_layout) or word the message.

Group A (2026-10-20, partial): LinuxHost readable openat2 now uses O_NONBLOCK|O_NOCTTY; a fast disposable-container FIFO probe with no writer and a 2s alarm is added to catch regressions. All LinuxHost statx calls now use AT_NO_AUTOMOUNT, including proc magic links; proc magic-link target statx retains AT_STATX_DONT_SYNC. docs/safety.md records the remaining dead-NFS kernel lookup/deadline limit and autofs path-traversal limit. This is not TASK64 completion: B-K are separate, and D5 cannot be bounded by a cooperative deadline while a synchronous kernel syscall waits. Focused E2E and review still pending at note time.

Group A verification update (2026-10-20): disposable Nix container `just e2e-one host_read_open_of_fifo_does_not_wait_for_writer` passed 1/1 after correction. Initial test incorrectly expected F_GETFL to retain O_NOCTTY; it does not, so the container now tests FIFO nonblocking behavior and the pure host unit test checks the O_NOCTTY flag mapping. No diskgc reclaim was run on a developer host.

2026-10-20 group K candidate: sandbox_gate_flag_is_enabled_nowhere now scans .cargo/config and config.toml throughout checkout directories, including symlinked .cargo directories without following arbitrary symlink trees; nested rustflags/alias and symlinked config regressions bite. Focused Nix sandbox tests 13/13 and fmt passed in isolated worktree. Integrated full E2E and independent QA/MPED review are pending; TASK64 B-J remain open.

2026-10-20 group B candidate: LinuxHost root-relative statx now opens with RESOLVE_NO_SYMLINKS and stats the held fd, preserving final-symlink identity while refusing intermediate symlinks. A mount-point final symlink returning ENOTDIR is classified unsafe_path through a no-symlink follow-up stat. Red-before/fixed-after host tests and focused disposable Nix-container host_resolve_refuses_symlinks passed 1/1; interlock canary refused host exit 86. Integrated full E2E and QA/MPED review pending; TASK64 C-J remain open.

2026-10-20 integrated gate: group B and K changes passed nix develop -c just e2e (Nix image 139/139, native Docker 6/6) on the combined staged tree; parallel QA and MPED reviewers found no blocking issue. AC2 and AC11 checked. TASK64 remains open for groups C-J and remaining group A caveat. No diskgc execution on developer host or real project paths.

Group C candidate (2026-10-20): LinuxHost::new now fstatfs-checks the held /proc directory against PROC_SUPER_MAGIC before reading NSpid or opening a pidfd, with a contextual refusal for other filesystems. In a disposable Nix container, host_procfs_stays_pinned_after_overmount passed 1/1: the probe unshared a private mount namespace, mounted its own procfs, opened a host on it, mounted tmpfs over /proc, verified a new host specifically refused the non-procfs mount, then lazily detached the overlay and the held procfs; readers through the held fd continued listing the probe. The probe cannot model an ancestor PID namespace procfs underneath, so docs/safety.md names that limit. Interlock canary refused host execution (exit 86). Full integrated E2E and independent QA/MPED review are pending; AC3 remains unchecked until integration.

2026-10-20 integrated group C gate: nix develop -c just e2e passed Nix image 140/140 and native Docker 6/6; parallel QA and MPED reviews GO. AC3 checked with the exact ancestor-namespace construction limit documented in docs/safety.md. TASK64 remains open for D-J and remaining group A caveat; no developer-host diskgc run.

2026-10-20 group D candidate: Delete::fchmod port now accepts only a held directory fd and deadline, reads current mode on the fd, checks the deadline immediately before fchmod, and requests only owner write addition. The caller no longer supplies arbitrary mode bits. ModelHost trace pins Now immediately before Fchmod; focused ModelHost checks 23/23 and engine 40/40 passed. Disposable Nix-container E2E passed rename-out 1/1, file bind-mount after statx 1/1, and concurrent chmod before fchmod 1/1; host interlock refused exit 86 each time. Linux kernel may clear setgid and chmod between fstat/fchmod is still a non-atomic race; docs/safety.md records this. Integrated full E2E and independent QA/MPED review pending before AC4 check.

2026-10-20 integrated group D gate: commit fdbbcff passed nix develop -c just e2e (Nix image 145/145, native Docker 6/6), including rename-out, bind-over-file and concurrent chmod probes. Parallel QA and MPED final reviews GO. Fchmod is limited to held directory fd plus deadline, and ModelHost no-op trace and ENOTDIR deadline precedence have regressions. AC4 checked; remaining groups and documented non-atomic fstat/fchmod caveat still block TASK64 completion.

2026-10-21 group E candidate: Clear is a single-use borrowed capability holding the exact Candidate and WalkRecord. WalkRecord records candidate path and mount at walk; Checker::check refuses another hard-linked name or mount before scanning, while delete_cleared consumes the checked pair. The engine-only bypass is named delete_unchecked and Checker::with_options is crate-private. ModelHost can replace a regular file with a fresh birth time at the same inode number when no aliases or references remain; model tests pin both the birth-time catch and the documented missing-birth-time limit. A chown at candidate reopen stops before unlinks. Focused Nix suites cargo_reclaim 19/19, engine 42/42, inuse 39/39, fs_dry_run 48/48 and core/model/e2e Clippy passed. Full integrated E2E and independent QA/MPED review remain pending before checking AC5.

2026-10-21 group H candidate integrated: LinuxHost lock test uses post-exec child lock contention with guarded bounded child lifetime; E2E release-during-wait probe requires Busy before release and measures wait. Rare original #37 root cause unconfirmed. Format and diff checks passed; stress, full E2E and independent review pending. AC8 remains unchecked.

2026-10-21 group I candidate integrated: reject non-normalized or duplicate fixed roots and idle_for:0s at load/bind, dedupe safely resolved root identities during fs/Cargo discovery, add focused regressions and docs. Nested roots still traverse separately with candidate identity dedupe. Format and diff checks passed; full E2E and independent review pending. AC9 remains unchecked.

2026-10-21 group J candidate integrated: LinuxHost read_dir uses O_NOATIME with EPERM fallback; deleted-file accounting counts a shared inode once while retaining all holders, and maps mounted btrfs subvolumes sharing mountinfo source to the requested filesystem. Narrow host/model regressions and btrfs visibility limits documented. Nix fmt, diff check, and cargo check --workspace --tests passed in isolated worktree; tests, integrated E2E and independent review pending. AC10 remains unchecked.

2026-10-21 group G candidate integrated: Checker rescans for every candidate after an unsettled scan; Linux reader uses pidfd signal 0 to recognize released processes on kernels where poll lacks hang-up and polls briefly before another incomplete round. Model/Linux regressions added for 16-listing bound, rescan, release and poll. Nix cargo check --tests, fmt and diff checks passed in isolated tree. Full E2E, old-kernel and loaded-CI measurements, and independent review pending; persistent fork storm still conservatively reports in_use. AC7 remains unchecked.

2026-10-21 integrated gate: E/I/J changes passed full just e2e (152/152 Nix-container, 6/6 native Docker), workspace tests including 40k candidates, and parallel QA/MPED review. Mixed-root preflight now skips foreign devices; protection and source root discovery screen full mount ancestry before opening, with ModelHost and dead-FUSE container regressions. AC5, AC9 and AC10 checked; A, F, G, H and final release review remain open.

2026-10-21 Group A audit: LinuxHost opens read targets with O_NONBLOCK|O_NOCTTY (host.rs:612-630), proc fd link stat uses DONT_SYNC (host.rs:472-485), and statx suppresses automount (host.rs:1105-1145). FIFO container probe host_suite.rs:83 bites the blocking-open case. docs/safety.md:45-46, 590-599, 874-885 and 1636-1653 explicitly state remaining dead-NFS/autofs/kernel limits. Thus AC1's fix-or-document wording is met; this does not claim all blocking filesystem calls can be interrupted.

2026-10-21 group H stress evidence: built diskgc-linux unit-test binary with `nix develop -c cargo test -p diskgc-linux --lib --locked --no-run`, then ran `host::tests::a_lock_conflicts_until_its_fd_is_closed --exact --test-threads=1` 600 times at xargs -P4 inside a disposable rootless Podman Debian container, with a 15s per-iteration and 180s outer bound. Result: 600/600 runs, 0 failures, 8.9s wall time. No diskgc deletion or host cleanup. `nix develop -c just e2e-one host_flock_release_only_by_close` passed 1/1 in a disposable Nix container (2.32s); release probe recorded Busy before release, 300ms deadline at 300ms, release-during-wait acquisition at 350ms, and clean deletion oracle. The historical 1/600 failure did not reproduce; exact original cause remains unconfirmed. AC8 stays unchecked pending integrated review/decision on that uncertainty.

2026-10-21 PID identity integration: Host now pins each process incarnation for the scan lifetime (Linux held pidfd, ModelHost monotonic generation). Scan detects release/replacement even when numeric PID listing is unchanged, relists after a listed process vanishes before identity open, retains observed uses, and treats pin/check errors or persistent churn as unsettled/in_use. ModelHost regressions include unchanged-number reuse, vanished-before-pin inherited child, and injected pin/check failures. Full just e2e passed 167/167 Nix and 7/7 native Docker. TID reuse within one clock tick remains explicitly documented in docs/safety.md and uncorrected; AC6 remains open pending release safety decision. H historical 1/600 root cause remains unconfirmed despite 600/600 stress.

2026-10-21 Group F availability limit after MPED review: the anchored task listing temporarily holds one procfs directory fd per listed thread, and TaskStats retains one fd per encountered tid until that process read ends. If a highly threaded process reaches RLIMIT_NOFILE, an open can return EMFILE; ProcError classifies it Failed, so the in-use scan fails closed and reclaim may be delayed. No threshold or old-kernel behavior was measured for this limit. docs/safety.md records it. A direct fake-procfs numeric-TID inode-mismatch test was added; the full gate for these followup edits is pending.

2026-10-21 group F TID integration: each listed TID is pinned by a held procfs task directory through count/listing validation, and task stat/owner reads use the held directory across read brackets. Fresh numeric lookup plus held inode identity catches listing ABA; vanished or unreadable task identity fails closed. Fake same-tick TID replacement and numeric-listing ABA tests bite; real-kernel held-task release probe passed 100/100 in disposable Podman. Stable-tree full nix develop -c just e2e exit 0: Nix container 167/167, native Docker 7/7, Linux unit 138 passed/1 ignored, fmt/clippy passed; QA and MPED reviews GO for this slice. Older-kernel behavior and forced real-kernel same-tick recycle remain untested; RLIMIT_NOFILE can make highly threaded scans conservatively fail. Group G old-kernel/load and H historical lock failure remain open, so TASK64 stays In Progress.

2026-10-21 read-only G/H plan: container images do not change the host kernel. To verify Linux 5.8-6.8 pidfd behavior, run disposable-container probes on actual hosts/VMs with verified uname -r (5.8, 5.15, 6.1, 6.8, and 6.9+ control): retain pidfd after child exit/reap, compare poll/HUP and pidfd_send_signal(0)==ESRCH, then release-during-scan E2E. For loaded fork churn, cap pids/duration and compare 0/1/4/8 worker levels for 30s, recording listing counts, conservative skips, wall time and fixture-only unlink; after storm, retry should settle. Historical flock 1/600 failure remains unexplained despite revised post-exec guarded holder and 600/600 stress; these are mitigation evidence, not root-cause proof. If old-kernel runners unavailable, AC7 and release gate remain open absent reviewed kernel-support scope decision.

2026-10-22 G/H live audit: added host_inuse_scan_settles_after_fork_churn to the disposable Nix-container E2E suite. The probe measures LinuxHost scan and Checker at 0/1/4/8 child-spawning workers (each capped at 64 spawns and 500 ms) and requires an unused candidate to clear after all children are reaped. On the current 7.2.9 host kernel, focused runs passed: one run showed 0 workers 2 listings/1 ms, 1 worker 4/6 ms, 4 workers 16/24 ms unsettled with checker retry and later clear, 8 workers 16/25 ms unsettled with checker retry and conservative block; post-storm 2/0 ms and clear. Another run again reached 16 listings at 4 and 8 workers, then cleared after the storm. This measures current-kernel behavior only; no old-kernel pidfd proof or prolonged 30-second loaded-CI sample. H audit: current holder takes the flock after exec in a guarded bounded child; E2E wait probe proves Busy before release and measures acquisition. Prior 600/600 parallel stress passed, but the original 1/600 cause was never captured; no root-cause claim or extra code fix. Keep AC7/AC8 open pending old-kernel and release review.

2026-10-22 G review correction: the live probe now counts child launches and completions before and after each raw scan and Checker call; at least one raw scan must overlap both a launch and a completion. It asserts a raw unsettled scan blocks its candidate, and an unsettled final Checker scan also blocks; the retry line is diagnostic because it may settle during the storm. Focused disposable Nix-container test passed after this change: 1 worker launched/completed 5/5 during the raw scan; 4 workers 84/85 and 8 workers 88/87, with both raw scans hitting 16 listings and blocking. The 8-worker final Checker scan was unsettled and blocked; post-storm scan cleared. Each worker stops launching after 500 ms or 64 launches, but these are not hard child-runtime limits; the E2E runner bounds the whole test. Old-kernel coverage and the historical flock failure remain unresolved; AC7/AC8 stay open.

2026-10-22 final G overlap-proof correction: counter snapshots alone could include a child event after scan return if the probe thread was descheduled. The disposable-container probe now timestamps before spawn, after spawn, and after wait using the same CLOCK_BOOTTIME representation as LinuxHost; it requires an entire successful child spawn-and-wait interval to fall within Scan::started() through started()+took() for at least one raw scan. Focused test passed on the current 7.2.9 kernel: 4 workers had 61 such complete intervals and 8 workers had 59; each raw scan hit 16 listings, reported unsettled and blocked. The final Checker result was also unsettled and blocked at 4/8 workers; post-storm scan cleared. This is current-kernel evidence only, with AC7/AC8 still open.

2026-10-22 Group G old-kernel evidence: added a Nix QEMU check pinned to linuxPackages_6_6 (guest uname 6.6.158). A guest Python probe holds a pidfd while a child is live, then reaps it and confirms poll returns POLLIN without HUP while pidfd_send_signal(0) gives ESRCH. The existing LinuxHost host_inuse_scan_settles_after_fork_churn E2E passed inside a disposable Podman container on that guest: 4 and 8 workers each reached 16 listings and conservatively blocked, then a post-storm scan cleared. Podman used its private guest ext4 graph root and only read-only Nix store/test-binary binds. Focused nix build .#checks.x86_64-linux.vm-old-kernel-pidfd passed; exact just e2e and independent QA/MPED review pending. This is one kernel point; 5.8, 5.15, 6.1, 6.8, a 6.9+ control and prolonged loaded-CI measurement remain open. Keep AC7 and DoD unchecked.

2026-10-22 Group G old-kernel evidence review correction: the first 6.6 VM check established kernel poll=IN/no HUP and signal-0 ESRCH, but fork-churn alone did not directly require LinuxHost to take that fallback. Added host_pidfd_release_after_reap: it pins a pipe-blocked live child through LinuxHost::proc_identity, requires proc_identity_released=false, reaps the child while retaining the pidfd, and requires proc_identity_released=true. The guest Python syscall probe runs first on 6.6.158, so the LinuxHost true result is a direct fallback bite; an always-false implementation fails. Focused Nix 6.6 VM check passed with live_released=false reaped_released=true, and the fork-churn case again reached 16-listing conservative blocks and cleared after the storm. Full just e2e and independent QA/MPED re-review pending. AC7 and DoD stay unchecked; other kernel points and prolonged load remain.

2026-10-22 Group G kernel matrix followup: the Nix QEMU pidfd check now accepts pinned kernel packages/version/HUP expectation and runs on 6.1.189, 6.6.158, and 6.12.112. All three focused Nix VM builds passed after adding two guest vCPUs so fork workers can overlap the scanner. Direct guest syscall probe after child reap observed poll mask 1 (POLLIN without HUP) on 6.1 and 6.6, mask 17 (POLLIN|POLLHUP) on 6.12; pidfd_send_signal(0) returned ESRCH on all. In each guest Podman container, LinuxHost held-identity test returned live=false/reaped=true, and fork-churn case passed with at least one complete child spawn-and-wait interval inside a raw scan and post-storm clear. Initial 6.1 single-vCPU run failed only the overlap precondition (all scans 0-1 ms); no assertion was weakened, and two-vCPU reruns passed. The VM uses private ext4 Podman graph storage and read-only Nix store/test-binary binds. Exact just e2e and independent QA/MPED review pending. AC7/DoD stay open for 5.8, 5.15, 6.8, prolonged loaded-CI sample, and other remaining TASK-64 work including H.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
