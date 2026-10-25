---
id: TASK-165
title: >-
  Step-4 recheck judges a stale path: an ancestor renamed after reach escapes
  keep-marker, checkout and protect rechecks
status: Done
assignee: []
created_date: '2026-10-25 01:12'
updated_date: '2026-10-25 05:33'
labels:
  - release-blocker
  - safety
  - hardening
dependencies: []
references:
  - docs/safety.md
  - crates/diskgc-core/src/fs_kind/judge.rs
  - crates/diskgc-core/src/fs_kind/reclaim.rs
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-13 async QA (pinned b009e59, read-only review; confirmed by reading, NOT reproduced: no vector was written, per QA scope).

Sequence (fs kind; user profile zephyr-* under /tmp shown, any fs candidate below a root applies):
1. Plan: candidate /tmp/p/zephyr-a found; /tmp/keep/.diskgc-keep exists.
2. reclaim: Judge::approach -> reach() opens /tmp, /tmp/p by fd and matches the candidate identity (Reached.dirs).
3. Another process (the user): mv /tmp/p /tmp/keep/p   (or mv /tmp/p ~/src/p: out of the root entirely, or into a VCS checkout).
4. lock_candidate; Judge::again -> place(found.path) checks the OLD path text; no_keep_markers(reached) statx'es .diskgc-keep in the OLD dir fds (/tmp and p), never /tmp/keep. checkout_metadata_below also works on the old path text. Judge::contents/engine::walk go through reached.parent() (old fd) and pass.
5. clear_and_delete deletes /tmp/keep/p/zephyr-a.
protect/!profile recheck (policy -> overlaps_checked_until) resolves found.path: fails closed (run stops) when protect matches are non-empty and the old path is gone; passes if the old path was recreated (two events) or protect is empty.

Contract broken: docs/safety.md Principles ("every check below runs on the current filesystem state immediately before acting") and Deletion step 4 ("the keep markers on the way again"). The documented object-scope limit (Deletion: Scope) covers a checked directory renamed MID-DELETE, not an ancestor renamed before the step-4 recheck; the window here is lock + whole locked walk (proportional to candidate size). Same class in cargo-target: markers between root and target are checked only by reach in step 1, before the locks (safety.md cargo Reclaim step 2 implies otherwise).

Decide: (a) fix: after the locked walk (and again inside clear_and_delete's also-callback) re-run reach from the root and require each dir's (dev, ino) to equal Reached.dirs, then no_keep_markers/place on the fresh chain (cost: one statx per level); or (b) document ancestor renames as an accepted limit next to object scope. Either way the docs and code must agree before 0.1 ships (irreversible contract).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A vector in crates/diskgc-model/tests/vectors/races/ renames an ancestor of the candidate into a directory holding .diskgc-keep after the flock of the candidate; it expects kept and nothing removed (or the limit is documented and the vector pins the documented outcome)
- [x] #2 Same check for a rename out of the root and into a VCS checkout
- [x] #3 cargo-target: keep markers between root and target are rechecked under the locks, or the doc says they are not
- [x] #4 docs/safety.md Deletion step 4 and Principles match the code
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented on branch t165 (commit f25e97b, not merged), 2026-10-25 04:27-~07:30. fs: still_reached (judge.rs) resolves the root anew and statx'es each name in the held directory above it, requiring the held directory, then the candidate identity, then keep markers and path rules; called in Judge::again (step 4, before the locked walk), in the clear_and_delete callback before protect and again as its last check, and before a command action starts (command.rs). cargo: fresh reach after the in-use check under the locks (Missing -> changed). Residual window documented (safety.md Principles, Deletion step 4/limits, cargo step 2/Limits) and pinned by races/ancestor-renamed-during-delete-is-the-stated-limit.yaml. Evidence: 13 vectors in crates/diskgc-model/tests/vectors/races (9 written first fail on master: deleted); each new guard mutated off -> a vector fails; container cargo_reclaim_changed_between_discovery_and_locking@{1.90.0,1.99.1,nightly-2026-10-16} PASS, and with the cargo recheck removed the oracle reports DELETED on 1.99.1. just check exit 0; flake e2e-clippy + e2e-tests exit 0. Full just e2e not run. Reviews: mped-architect (no blocker; adopted: last-check-before-act, Principles clause, stated-limit vector, cargo replaced-path vector) and codex (no blocker; adopted: root gone -> changed). Not done (follow-up): cargo per-part path recheck (cost: reach lists every ancestor dir per part); fs container test (driver has no fs-kind reclaim operation).

Merged to master as 2f8fe73 on 2026-10-25T05:33Z (rebased over TASK-173 without conflict; just check exit 0).
<!-- SECTION:NOTES:END -->
