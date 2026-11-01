---
id: TASK-268
title: >-
  Overlay mounted inside a protected or negated match escapes the overlay
  refusal
status: To Do
assignee: []
created_date: '2026-10-31T03:39:00Z'
updated_date: '2026-11-01T02:43:00Z'
labels:
  - safety
  - irreversible
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Full review 2026-10-31 on master 6f14dd7 (tmp/reviews/codex-full-6f14dd7.md; mped full review in the TASK-266-window notes); static reading, not reproduced. Codex P1, static config, 0 events: protect /proj/target; an overlay at /proj/target/debug/deps/view with upperdir /cache/upper; select /cache/upper/cruft. Cargo discovery does not descend into deps/view, both compared paths are ext4, refuse_overlay passes, places differ by device; deleting the upper changes the protected subtree. executor/policy.rs refuse_overlay / overlaps_in_filesystem. Fix: include mounts nested beneath protected/negated matches (also through aliases) in the overlay uncertainty check; refuse unless backing-directory overlap is ruled out (parse upperdir/lowerdir or refuse). Model test red first.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Branch t268-overlay-protect (edd2e01, c9433c8), not merged. Model: MountSpec.overlay_upper (upper dir shown under the overlay's own dev, root field /). Rule (safety.md, Not protected, not excluded): with any match, an overlay or a mount-screen-refused type (FUSE, network, autofs) mounted at or below a match (by path, or by a place on its parent chain inside the match's place, through binds) is an error for every candidate; upperdir/lowerdir not parsed. Red first: vectors config/overlay-mounted-inside-a-{protected,negated}-match (dry run planned the candidate, exit 0; reclaim deleted /proj/target/debug/deps/view/cruft/blob). Container: protect_refuses_an_overlay_mounted_inside_a_protected_cargo_target PASS; with the rule bitten the oracle saw /tmp/ovp/cache/upper/cruft DELETED (real-kernel reproduction). Gate: nix develop -c just check exit 0.

Review round on 259b65e (Codex + mped), branch rebased on master bdc6e9c: MUST 1 (Cargo target dropped when an entry vanished during enumeration: bounded re-inspection, unsettled = refused) 8edb9f5; MUST 2 (ecryptfs/aufs/shiftfs nested only; own-mount limit documented) 6d38e8c; MUST 3 (loop images via new read-only Host op loop_backing_file, major 7 only) 25e22b7; MUST 4 (CACHEDIR.TAG on the way down triggers re-enumeration in DeletePolicy) 63c49aa. All red first (vectors under tests/vectors/config). Gates: just check exit 0; e2e-clippy + e2e-tests exit 0; e2e-one green for the overlay test, 6 protect/negation tests, the Cargo vector test and 4 Cargo @1.99.1 tests.
<!-- SECTION:NOTES:END -->
