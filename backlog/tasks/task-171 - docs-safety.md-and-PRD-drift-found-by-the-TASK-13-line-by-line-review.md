---
id: TASK-171
title: docs/safety.md and PRD drift found by the TASK-13 line-by-line review
status: To Do
assignee: []
created_date: '2026-10-25 01:14'
updated_date: '2026-10-25 12:49'
labels:
  - hardening
  - docs
dependencies: []
references:
  - docs/safety.md
  - PRD.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-13 async QA (b009e59, read-only line-by-line review of docs/safety.md vs code; this is TASK-13 AC #2 material). Each item: fix the doc or the code so they agree.
fs kind:
1. Deletion step 4 / Principles: keep markers and place re-judged "as it is then" - code reuses step-2 fds and the stale path (TASK-165).
2. Walk ENOENT: doc says changed; engine::walk maps ENOENT on the candidate's own first statx to missing.
3. Mount-race limit text says "the delete's EXDEV"; a mount on a file shows as EBUSY from unlinkat.
4. Vector comment in races/mount-made-during-the-locked-walk.yaml ("only the walk's own per-entry mount id check sees it") is false: RESOLVE_NO_XDEV in Stack::open catches it (why TASK-129 M7 survives).
5. Principles exclude "renaming or replacing entries inside a candidate"; renaming an ancestor is neither excluded nor a stated limit.
In use / liveness:
6. Owner PID step 8: "During planning and tallying, again under the candidate lock, and immediately before deletion" - the tally runs only name_filter (no scan); the Order paragraph is right, so the doc contradicts itself.
7. Owner PID step 3: "At startup diskgc checks self/ns/pid" - code reads pid_namespace() before the first listing and after NSpid reads (scan::pid_table) only when pid_lookup is set; no startup check (code stricter).
8. Freshness: the clearing check reads the clock "as its last step before the deletion starts" - liveness veto, app::check and runtime_retention_guard_veto run after it; delete_cleared re-reads the clock (safe, wording wrong).
9. In use first bullet omits exe (How::Exe is matched).
10. Visibility: nothing on hidepid or a non-initial pid namespace for the plain scan (PRD Risks lists hidepid) - TASK blind spots.
cargo-target:
11. Reclaim step 2 "or no longer known: changed" - judged_last_build is None in ensure (TASK-168).
12. Reclaim step 3 "every walk refuses every Cargo lock name" - only known names everywhere (TASK-167).
13. Step 1 order: code judges checkouts, then judge_layout on the discovery-time inspection, then reach; doc puts reach first (a kept+unsupported target gets a different reason in ensure-untallied vs dry run).
14. Step 2 "no keep marker": markers between root and target are checked only by reach in step 1, before the locks.
15. The protect/negated recheck under the locks and before each part (guard in reclaim_checked) is not in the Reclaim protocol section.
16. In-use blind spots list: add build.build-dir / CARGO_BUILD_BUILD_DIR pointing at an attached target, and a target symlink into another project's target (LD_LIBRARY_PATH names the symlink path).
17. "cargo clean is safe ... both only delete": clean removes the lock files, so a following build reopens the profile dir diskgc is deleting in with new lock inodes (two outside events; noticed at the next part).
PRD (needs Mark):
18. "a target deleted while a Cargo process holds any of its locks" is stricter than enforced: a newer Cargo creating .cargo-build-lock or a first release build creating release/ locks mid-part holds a lock of the target while diskgc deletes debug/. Safe by the .cargo-lock invariant; state the enforced property ("no Cargo writes into a profile directory while diskgc deletes there; diskgc holds every lock that profile had when its deletion started").
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Items 1-17 resolved (doc or code) with the linked tasks
- [ ] #2 Item 18 ruled by Mark and PRD updated if accepted
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
COMPASS round 3 (2026-10-25): the 0.1 subset (items 1, 11, 12 and TASK-168's doc fix) is a 0.1 blocker; the rest is post-0.1.

0.1 subset re-checked 2026-10-25 against master 3367f82 (t171, no doc or code change needed; branch t171 has no commits):
- Item 1: already fixed by TASK-165 (2f8fe73). Principles says checks run "on the path as it is then (a directory on the way renamed since it was reached is changed)"; Deletion step 4 states the path recheck. Code agrees: fs_kind::judge::still_reached (root resolved anew, statx per level against Reached.dirs, then no_keep_markers on the held dirs) runs in Judge::again (step 4) and as the last check in the clear_and_delete callback in fs_kind::reclaim (after policy, liveness::veto, app::check, runtime_retention_guard_veto); cargo_target::reclaim::still_reached under the locks.
- Item 11 / TASK-168 doc fix: already fixed (b5bcb7a). Reclaim step 2 says ensure hands no earlier last build; code: executor walk_cargo passes ReclaimPolicy judged_last_build: None; only the model test passes Some.
- Item 12: already fixed by TASK-167 (b5bcb7a), code changed: engine::walk refuses any is_cargo_lock_name (.cargo-*lock*) at any depth as contains_special; Reclaim step 3 and Filesystem candidates (No special content) state that rule.
- Gate on unchanged tree: nix develop -c just docs-check exit 0 (332 core lib tests); python3 tools/check_docs.py docs/reference.md docs/safety.md PRD.md exit 0.
- Item 18 (PRD, needs Mark), proposed wording, PRD.md "What bad looks like":
  - Any Cargo lock file deleted or moved; a target deleted while a Cargo
  -   process holds any of its locks; two Cargo builds running in one target
  -   dir because of diskgc; anything outside locked profile directories
  -   deleted by the Cargo kind.
  + Any Cargo lock file deleted or moved; an entry of a profile directory
  +   deleted unless diskgc holds every lock that profile directory had when
  +   its deletion started, or while a Cargo writes into that profile
  +   directory; two Cargo builds running in one target dir because of
  +   diskgc; anything outside locked profile directories deleted by the
  +   Cargo kind.
  and in "What good looks like":
  - - Cargo targets only inside profile directories whose locks diskgc holds;
  + - Cargo targets only inside profile directories whose locks diskgc holds
  +   (every lock the directory had when its deletion started);
  Rationale: the current text is stricter than enforced. A newer Cargo creating .cargo-build-lock, or a first release build creating release/ and its locks, mid-part holds a lock of the target while diskgc deletes debug/. Safe by the .cargo-lock invariant (safety.md Reclaim step 3: such a Cargo waits on the .cargo-lock diskgc holds having written nothing else, or works in a profile directory diskgc does not touch; diskgc stops at the next part as changed or in_use). Stopped here for the decision.
- Post-0.1, left open: items 2-10, 13-17 (not re-checked).

Item 18 decided 2026-10-25 by mark-emulator (Mark may veto): take the precise wording ('the PRD says what we guarantee, not more'); applied to PRD.md (Safety boundaries Cargo bullet and What bad looks like). 0.1 subset (1, 11, 12) already fixed by TASK-165/167/168 merges. Post-0.1 items 2-10, 13-17 remain.
<!-- SECTION:NOTES:END -->
