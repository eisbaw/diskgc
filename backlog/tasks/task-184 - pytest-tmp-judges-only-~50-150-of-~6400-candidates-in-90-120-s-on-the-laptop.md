---
id: TASK-184
title: pytest-tmp judges only ~50-150 of ~6400 candidates in 90-120 s on the laptop
status: Done
assignee: []
created_date: '2026-10-25 05:34'
updated_date: '2026-10-25 09:19'
labels:
  - shipping
  - performance
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-166 host read-only dry runs (2026-10-25, ~3500 processes, load high): ensure --dry-run --select profile:pytest-tmp /tmp judged only about 50-150 of roughly 6400 candidates in 90-120 s; an earlier 300 s run judged 446. pytest-tmp is the main user value of 0.1 (~17.5 GiB on Mark's laptop, TASK-112 step 5 host dry run took 16:35 for 6401 sessions). Profile where per-candidate time goes (in-use scan per candidate vs per batch; pytest liveness facts; pytest-of listing every check) and cut it at the root, within the Freshness rules. Walk with the release artifact on a host-shaped load and many sessions (just walk-up NAME, WALK_TMP_BULK).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
COMPASS round 3 (2026-10-25): the slow baseline was likely measured with forgotten walk envs burning ~4 cores and ~3500 processes; re-baseline at the laptop's normal ~600-660 processes, load recorded, no other walk envs, before accepting the number.

2026-10-25 implementer (branch t184, worktree tmp/worktrees/t184; started 06:42 CET). Commits: 07e840b (fix), 8df3a49 (review nits), 8c1c042 (ignored red model test for the protect cost). Raw data: tmp/t184/ (host runs in tmp/t184/host/, perf/strace files), walk logs tmp/walks/2026-10-25-t184/.

WHERE THE TIME GOES (host, read-only, static and glibc builds of master 4c86d74):
- Dry run = discovery, then per session: approach + eligibility walk + liveness. Scans are NOT the plan's cost: the plan checker reuses a scan for 60 s (strace: 3 scans in a 150 s run).
- Discovery: /tmp holds ~191k entries; temp-prefix discovery visits ~145k directories (nix-shell.*, nix-develop-*, ...), 2 opens + 2 statx each: ~30 s warm, 75-110 s cold or under strace. At a 90-120 s deadline the plan had only just started judging: that is the original '50-150 judged' symptom.
- Sessions: 6401, 7.75 M entries in their trees (find alone: 178 s unniced). Each is walked in full (sizes, newest change, idle): I/O and sys time, inherent to sizing.
- CPU: perf (user) put 53% of user time in pytest row 6 (liveness::pytest::naming): every argument and environment value (and ':'-part) of ~330 own processes (~4.5 MB of environments) normalised again for every session, ~65 ms/session.
- pytest-of listing per check: P holds ~4 names; negligible. Liveness facts are already read once per scan.

CHANGE (07e840b, 8df3a49): Scan holds pytest::Processes (facts + per-process normalised named paths, derived once in Processes::new, only for strings holding 'pytest-', exact because normalisation copies every kept name and S's own name is pytest-<N>); row 6 compares bytes against S's normal-form path (asserted). Property test against the old definition (oracle), both argument and environment sides; mutations of the boundary rule and of the byte filter fail it. No rule, check or Freshness change.

BEFORE/AFTER, host (read-only; static release candidates: before ym22mk2a = master, after idqfsjmr = 07e840b; `nice -n 19 ionice -c 3 diskgc ensure --dry-run --json --no-index --timeout 30m --min-free 100% --select profile:pytest-tmp /tmp`, interleaved, all 6401 judged, exit 0):
  before-1 07:56 load 19.7 procs 1005: 1282 s wall, 352 s user, 177 s sys
  after-1  08:17 load 21.3 procs 950:   914 s,  45 s user, 160 s sys
  before-2 08:32 load 9.0  procs 950:   804 s, 231 s user, 117 s sys
  after-2  08:46 load 7.9  procs 941:   613 s,  27 s user, 104 s sys
  before-3 08:56 load 7.0  procs 933:   791 s, 216 s user, 113 s sys
  after-3  09:09 load 8.2  procs 938:   642 s,  28 s user, 107 s sys
Paired wall -29/-24/-19 %, user CPU -87/-88 %. Verdicts equal: eligible 6049-6052, future_time 331, contains_ipc 15, in_use 3-6 (live processes). Conditions: no other walk env (COMPASS stopped them 06:35Z); my own walk container (WALK_PID_HOST=1, 2 burners) was up throughout, hence ~930-1005 processes rather than 600-660; other implementers' builds not controlled. Remaining wall is I/O and sys (21-22% CPU under nice 19 / ionice idle): discovery plus 7.75 M entries.

RECLAIM (walk-diskgc-t184, release candidates, WALK_PID_HOST=1, WALK_TMP_BULK=8000: 304 sessions of 1 MiB; partial goals, 20-40 sessions per run, interleaved): when host scans are fast one scan already serves the whole reuse window: 5-33 ms per candidate, before and after alike (after 8-11, before 20-33 ms in the paired quiet runs). When host process churn makes scans slow or unsettled ('still growing after 16 listings'): 500-830 ms per candidate, one scan each. Container processes have small environments, so row 6 costs little there: the walk shows no regression, not the host's CPU gain.
Host scan cost now (just scan-timing, 1609 processes, load ~17-19): glibc median 336 ms (pytest facts), static median 514 ms, p95 1050 ms. With a session walk + delete of ~100-300 ms on this disk, the next check falls past 500 ms: a host reclaim pays one scan per session (~0.5-1 s), ~1-2 h for all 6400. INHERENT under Freshness as written (a scan serves checks strictly before 500 ms from its start). OPTIONS FOR A DECISION: (a) batch: lock and walk up to K candidates (bounded by a time budget), then one scan, then check and delete each while the scan is < 500 ms old at its check and < 1 s at its delete start (no rule change; holds K locks; widens walk->delete to the batch budget; gains only while scans are < ~400 ms, i.e. glibc or a quiet host); (b) extend TASK-173's 'one scan serves while each delete starts within 1 s' from Cargo parts to fs candidates (a rule change; gains while scans < ~900 ms); (c) cheaper scans (TASK-174 musl gap); (d) accept: a goal-bounded ensure stops early; best-fit order picks the largest first (but only ~30 s of tally, --tally-budget, is sized).

PROTECT / !profile (COMPASS's finding, CONFIRMED): every fs and Cargo candidate enumerates every protect (and negated) tree three times: protected_before_reclaim, under the lock, and in policy_skip inside the clear's freshness window. Walk with protect {cargo-target, roots: [/home/tester/src]}: 7,300 dirs: 263-617 ms/candidate vs 5-33 ms without; 61,000 dirs (tmpfs, load 4-7): 2.2-3.3 s/candidate and 1-4 in_use_unknown per 20 deleted ('its clear expired before the deletion could start (the scan of 920 processes took 252 ms)': the scan was fast, the policy enumeration was not). ~/topics on this laptop has 340k directories: every candidate's clear would expire. Model test scans_and_protect_enumerations_do_not_grow_with_candidates (8c1c042, #[ignore]): protect-root listings 7 for 2 candidates, 19 for 6 (red on master and branch); scans equal. NOT FIXED HERE: the suggested fix (enumerate once per tier, check overlap against that set) breaks command_union_rechecks_new_negated_match_just_before_invocation and two siblings, which pin that a negated match created inside the candidate after the locked judgement is seen before a COMMAND action runs, and it changes docs/safety.md ('rechecked under the candidate lock and immediately before deletion'). A draft of it is in tmp/t184/policy-cache-option-a.diff (unreviewed). Options: (1) per-tier set for `delete` actions only (a match created inside the candidate later is never unlinked: unwalked entries fail ENOTEMPTY; the residual is an ancestor that becomes a match, e.g. a new CACHEDIR.TAG making an ancestor a Cargo target), full recheck kept before command actions; (2) per-tier set everywhere, docs and the three tests changed (orchestrator's suggestion); (3) keep re-enumeration but move it out of the freshness window (fixes in_use_unknown, not the O(candidates x protected tree) cost). Filed as its own task for the decision.

REVIEW: mped-architect, one read-only round on 07e840b: ship, no blockers (nits fixed in 8df3a49).
GATES: `just check` exit 0 on the final tree (8c1c042) with the worktree's OWN target dir: 1129 passed, 0 failed, 2 ignored. Shared-target runs were unreliable (see follow-up): one 'pass' (on 07e840b) ran another worktree's test binaries; one failed compiling this tree's diskgc against another tree's diskgc-core. diskgc-e2e not touched; no e2e-one run (no container-test-relevant change).

Follow-ups filed: TASK-200 (protect/!profile enumerated 3x per candidate, decision), TASK-201 (shared CARGO_TARGET_DIR links across worktrees; gate evidence), TASK-202 (in_use_unknown detail blames the host when checks after the scan were slow), TASK-203 (one scan per candidate on a busy laptop: batch option or rule change), TASK-204 (discovery syscalls per directory). Status left In Progress for the orchestrator: the dry-run part is cut; the reclaim part is inherent under Freshness as written (TASK-203) and the protect part awaits TASK-200's decision.

Merged to master as 3ce3d51 on 2026-10-25T09:19Z. Partial: dry-run CPU -87%, wall -19..29%; remaining time is discovery and session-walk I/O; reclaim throughput continues in TASK-200 (protect per candidate) and TASK-203 (one scan per candidate).
<!-- SECTION:NOTES:END -->
