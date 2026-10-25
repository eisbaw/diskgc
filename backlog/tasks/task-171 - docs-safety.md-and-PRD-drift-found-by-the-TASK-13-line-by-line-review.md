---
id: TASK-171
title: docs/safety.md and PRD drift found by the TASK-13 line-by-line review
status: To Do
assignee: []
created_date: '2026-10-25 01:14'
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
