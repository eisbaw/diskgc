---
id: TASK-65
title: /tmp built-in profiles for 0.1 (verified)
status: To Do
assignee: []
created_date: '2026-10-17 23:55'
updated_date: '2026-10-24 08:24'
labels:
  - wave-2
  - profiles
dependencies:
  - TASK-17
  - TASK-112
references:
  - docs/reference.md#built-in-profile-library
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: 0.1 ships "the verified /tmp and Cargo built-ins" (PRD Scope, Releases), for the /tmp laptop owner (task-22). Pulled out of task-39: the verified /tmp subset only, all with the plain delete action.

Profiles (names from docs/reference.md, Built-in profile library):
- pytest-tmp: one candidate per pytest test dir, pytest-of-*/pytest-*/* in /tmp, /var/tmp, and the same one level down (nix-shell.* / tmp.* dirs); type dir, so the *current symlinks, plain files in a session dir, the session dirs and pytest-of-<user> are never candidates (task-71)
- nix-shell-rc: /tmp/nix-shell-*-*, /tmp/nix-develop-*-* (rc dirs left by every shell start)
- nix-shell-tmpdir: /tmp/nix-shell.* (a shell's TMPDIR)
- mktemp-dir: /tmp/tmp.*
- tmp-entry: any top-level entry of /tmp, /var/tmp
- containers-image-tmp (from the task-39 dogfooding note, 2026-10-17): /var/tmp/buildah*, /var/tmp/container_images_storage* (68.5G on the dogfood host, blobs left by buildah/podman). It is not yet in the reference.md table; adding the row is part of this task, and it ships only if verified like the others.

Dogfood findings to verify against (task-39 note): /tmp had 147,557 top-level entries (101k nix-shell rc dirs, 38k nix-shell.*), a full walk took 265 s at idle IO priority (the PRD's 100k-entry case); all pytest-tmp matches were nested inside nix-shell temp dirs.

Stays in task-39: claude-code-tmp, chromium-tmp, playwright-tmp, node-compile-cache and nix-user-cache (~/.cache/nix, not /tmp scope).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each profile ships in builtin.yaml with examples and golden match sets (per task-17)
- [ ] #2 Each profile is verified against the dogfood findings and carries verified_with from a real system
- [ ] #3 No profile depends on the Docker or Nix kinds, command actions or survey (task-37, task-38)
- [x] #4 Dormant nix-shell and nix develop sessions keep their rc and TMPDIR dirs (container test), or nix-shell-rc and nix-shell-tmpdir do not ship in 0.1
- [x] #5 pytest-tmp's checkouts setting is decided (survey 2026-10-19: all 75,253 leftover repos were test fixtures; recommendation: allow)
- [x] #6 pytest-tmp's granularity (per session or per test dir) is decided before it ships: with checkouts allowed, per session costs about a tenth of the candidates; once shipped, a changed match set needs a new name
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-68: open question, unchecked. The dry run found every nix-shell rc dir (/tmp/nix-shell-*-*, /tmp/nix-develop-*-*) eligible: 65,458 judged, no in_use, though live shells were running on the laptop. Nothing has them open and no environment names them, so the in-use check cannot see a live shell's rc dir. Is deleting a live nix shell's rc dir harmless (read once at startup?) or does a live shell re-read it? Check before nix-shell-rc ships (this task's AC #4 covers TMPDIR dirs, not rc dirs).

From task-71 D2 (Mark, 2026-10-18): pytest-tmp takes one pytest session per candidate (paths pytest-of-*/pytest-*, nix-shell.*/pytest-of-*/pytest-*, tmp.*/pytest-of-*/pytest-*; type dir), not the whole pytest-of-<user>: one session where a test ran git init (contains_checkout) no longer keeps the others. pytest's pytest-current symlink is never a candidate; pytest-of-<user> itself is left. reference.md (Profiles example, Built-in profile library row and note) and fs_kind/spike-profiles.yaml carry it; ship the built-in with these patterns (the name is not yet released, so no rename is needed). Dry run here: numbers in task-71's notes.

From task-71 round 3 (Mark, 2026-10-18): pytest-tmp is one candidate per TEST DIR (pytest-of-*/pytest-*/*, plus nix-shell.*/ and tmp.*/ one level down; type dir), superseding the per-session note above. The checkout guard stays intact; only test dirs that ran git init are kept. mped measured the per-session cost here: 1,559 of 4,670 sessions held a .git, about 21 GiB, mostly whole suites where a few tests ran git init. Open items for when the profile ships:
1. Check that pytest tolerates a test dir disappearing from a live session (or rely on in_use / an idle filter in the tier: a live session's test dirs are usually recent and often open).
2. (mped N4) pytest writes a .lock file holding its PID in a live numbered session dir; consider it as an in-use hint for the test dirs below it (today it is only a plain file in the session dir, never a candidate and not read).

Correction (task-80, from mped's round-3 review of task-71): the premise above ('mostly whole suites where a few tests ran git init') does not match the per-test-dir sample in task-71's notes. There, most of the bytes the guard keeps sit in the test dirs that ran git init themselves: 11,293 contains_checkout test dirs, about 53 per session that holds a checkout (212 such sessions in the judged part), 3.13 GiB by du, against 0.32 GiB of eligible sibling test dirs in those sessions. So splitting per test dir recovers the siblings only, about a tenth of what the guard keeps; the rest stays until pytest-tmp's checkouts setting is decided (task-80: it keeps the default, refuse, until the survey of its kept test dirs).

Decision (Mark, 2026-10-19, relayed by the orchestrator; AC #5): pytest-tmp says checkouts: allow. Evidence: a read-only survey of ALL 75,253 leftover repos in pytest sessions under /tmp on the development machine (not a sample) found only test fixtures, no real work: 64,042 authored by Test <test@example.com>, no worktrees, no network remotes; the only clones in the user's real identity were 50 switch_utils clones whose commits are all in the real repository. Sessions at most 19 days old, about 21.7 GiB. Landed with task-80: fs_kind/spike-profiles.yaml (the setting with this evidence as a comment), reference.md (the library row, the single home for which built-ins allow checkouts, and the pytest-tmp note), a model test (a test dir holding .git is eligible under pytest-tmp; bitten). Ship the built-in with checkouts: allow. Granularity stays per test dir (pytest-of-*/pytest-*/*) for now; it was chosen while the guard applied here and can be revisited now that it does not (the per-candidate cost is task-70's).

Dry run (task-80 round 2, 2026-10-19, this laptop, read-only: ensure --dry-run --select profile:pytest-tmp, spike-profiles.yaml with checkouts: allow, nice/ionice, --timeout 20m): exit 124 at the deadline, so partial. 75,142 test dirs judged: 75,066 eligible, 6.29 GiB (diskgc's estimate); 76 contains_ipc; 0 contains_checkout; every plan entry carried checkouts: allow. Its unit is the test dir; the survey's units are repositories and sessions (21.7 GiB), so the two sets of numbers are not comparable.

From task-80 r3 review (mped NITs): AC #6 wording should read 'per session yields about a tenth as many candidates'; the earlier implementation note saying spike-profiles.yaml carries the survey evidence as a comment is superseded (the comment is now a pointer; reference.md rests the allow on pytest's own retention, tmp_path_retention_count).

2026-10-20 per-test-directory decision is implemented in builtin pytest-tmp with checkouts: allow; pytest-xdist popen-gw<N> remains a worker-directory exception because it is the path at the matched depth. Full expanded temp-prefix v1 read-only host dry run was attempted with --min-free 500G --timeout 5m: 361,495 candidates discovered, 22,126 judged before deadline (exit 124); 22,116 eligible, 10 contains_ipc, 2,091,106,304 eligible bytes in the judged subset. The three largest extant eligible candidates matched du -s -B1 byte for byte. This is explicitly partial, so full profile DoD and TASK-65 remain open. TASK-70 throughput follow-up is required before calling pytest-tmp verified at host scale; no live deletion was attempted.

Active-session limit retained: a live pytest process may hold no descriptor in an individual test directory while still using it; bare profile:pytest-tmp can select that directory. Reference guidance now requires an idle filter and protect for active sessions. The existing held-file container test proves only the descriptor-held case. Safe deletion of the no-descriptor case remains unverified.

2026-10-20 full read-only pytest-tmp host dry run, after indexed environment-path optimization: 361,495/361,495 candidates judged in 350.56 s; 361,358 eligible, 137 contains_ipc, zero errors, 28,565,594,112 eligible bytes. Three sampled candidates matched du -x -B1 -s exactly. This was a warm-cache run following earlier scans; cold-cache completion under the default 10-minute timeout remains unproven. No live deletion. Integrated full E2E and final reviews still pending at time of this note.

Final integrated nix develop -c just e2e passed 93/93 on 2026-10-20; QA and MPED reviews GO for the opt-in built-ins. TASK-65 remains To Do: nix-shell-rc and nix-shell-tmpdir are unshipped pending dormant-session protection, and a live pytest session without an open descriptor can still make a bare pytest-tmp selection unsafe. The warm-cache full host dry run establishes plan completion here, not safe deletion of such a live session or cold-cache latency.

2026-10-20 disposable pytest 9.1.1 counterexample (Nixpkgs-pinned package in rootless Podman): with tmp_path_retention_policy=none, a live test created /tmp/pytest-of-root/pytest-0/test_active0 and no numbered-session .lock. During the sleep, /proc/<pytest-pid>/fd had no descriptor under the test directory. The same case is now a checked-in ModelHost negative YAML vector: an old live test dir is selected and deleted by the unsafe user-configured path rule; vector suite passed 4/4. The new Nix-image E2E starts real pytest as tester, asserts no lock/no candidate fd, and proves the unsafe user probe marks it eligible in dry run; oracle clean, focused E2E passed 1/1. Retired pytest-tmp from builtin.yaml and adjusted catalog/docs; no real-host diskgc run. A parent-lock predicate misses supported retention_policy=none, while age and initial-environ checks cannot establish a live no-fd test dir is unused. pytest-tmp remains planned pending a reliable session-liveness protocol. nix-shell-rc, nix-shell-tmpdir, mktemp-dir and tmp-entry remain unshipped due previously proved dormant-session visibility gaps; AC4 is satisfied by the unshipped branch. TASK65 remains To Do because AC1/AC2 and DoD are not met for all target profiles.

2026-10-22 Compass domain review: only containers-image-tmp is currently shipped from this proposed six-profile set, with fixed /var/tmp prefixes and a 3d source floor. Existing golden/model/Buildah-held-spool evidence does not test the exact release artifact or a dormant spool. pytest-tmp, nix-shell-rc, nix-shell-tmpdir, mktemp-dir and tmp-entry remain unshipped because checked-in no-fd/no-lock, post-exec TMPDIR and PID-namespace counterexamples defeat name, age, open-fd and host-PID heuristics as general liveness proof. The task has a scope conflict: AC1/2 say each proposed profile ships and is verified, while AC4 expressly permits two shell profiles not to ship. Do not check AC1/2 or close TASK-65 by treating an unsafe profile as safe. Resolve the signed-off 0.1 catalog scope with TASK-67 and prove that catalog using the actual release binary; any broad deferred profile needs a real liveness contract and disposable-container counterexample tests. No host deletion.

2026-10-23 TASK-65 exact built-in VM proof: the disposable vm-historical-ctime check passed after adding vm_containers_image_dormant_spool_exposes_liveness_gap. The existing exact containers-image-tmp vector proves free deletion and held preservation with the real binary and kernel ctimes; the new dry-run fixture checks the unchanged 3d floor using five-day-old kernel ctime, and gets free eligible, held in_use, fresh not_idle, and a live dormant spool eligible. The dormant fixture process reads and closes its spool at the old guest time, keeps no fd/cwd/mapping/initial environment reference while waiting, and successfully reopens the same bytes on resume. This is a negative liveness proof for a reusable spool, not evidence that actual Buildah or Podman reuses one. No developer-host diskgc run and no live-spool deletion. The real producer lifecycle remains unverified, so do not claim this profile release-safe or check AC1/AC2/DoD; TASK-65 and TASK-67 remain open. Focused command: nix develop -c nix build --no-link .#checks.x86_64-linux.vm-historical-ctime (exit 0); free space remained 29 GiB by df -BG.

2026-10-23 fixture correction after review: the dormant spool path is now fixed inside the VM helper, not passed in argv. The test requires a live /proc/<pid>/cmdline containing hold-reuse but no candidate path, in addition to checking no fd/cwd/root/exe/maps/initial-environment reference. Focused vm-historical-ctime check passed again on this final fixture; exact built-in free/held differential and dormant negative proof both PASS. docs/safety calls the source embedded because the proposed 0.1 catalog excludes it. No change to TASK-65 status or acceptance criteria.

Orchestrator 2026-10-23: now depends on TASK-112 (producer liveness contracts). Mark chose the full product over a Cargo-only 0.1, so /tmp built-ins are re-admitted only with a verified liveness contract.

2026-10-23 Mark (TASK-112 D1): pytest-tmp unit is one candidate per session dir pytest-of-*/pytest-* (under the temp-prefix v1 preset), superseding the 2026-10-18 per-test-dir decision recorded above. Reason: the liveness evidence (session .lock, session number) belongs to the session and pytest's own cleanup removes whole sessions; with checkouts: allow decided, the checkout-guard reason for the finer unit is gone; about a tenth as many candidates. pytest-tmp is admitted as a verified heuristic under liveness_contract: pytest (docs/safety.md Producer liveness). D2: containers-image-tmp withdrawn; re-admission via TASK-125.

2026-10-24 from TASK-112 steps 0-1: containers-image-tmp withdrawn from the built-in library (e7c0686), its matching set kept as the user-probe vector containers-image-age. Lifecycle facts pinned by container tests (f27e71c, tier full): legacy nix-shell's rc dir IS the TMPDIR of the shell and its children, and its rc EXIT trap removes it on normal exit or SIGHUP, so nix-shell-<pid>-* leftovers come from SIGKILL; nix develop's nix-develop-<pid>-* is empty, referenced by nothing, and always left behind (Nix 2.34.9). pytest 9.1.1 lock/rank/keep0/--basetemp facts confirmed as designed.

2026-10-24 from TASK-112 step 4 (557db0e): nix-shell-rc is now a built-in with liveness_contract: nix-shell (Owner PID) and min_idle_for 1h; its private-PID-namespace counterexample is refused (vector nix-shell-rc-namespace-local-pid-refused; container test nix_shell_rc_shell_in_private_pid_namespace_is_kept), and it is excluded from the release-01 candidate catalog. Host dry run (read-only, 125,215 rc dirs, 29 s): 124,952 eligible, 837.6 MiB; 23 in_use owner_pid; 59 producer_unknown (thread ids, TASK-131). nix-shell-tmpdir, mktemp-dir, tmp-entry stay excluded.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
