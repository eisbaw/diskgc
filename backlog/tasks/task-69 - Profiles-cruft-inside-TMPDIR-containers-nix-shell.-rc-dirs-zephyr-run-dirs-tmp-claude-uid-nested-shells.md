---
id: TASK-69
title: >-
  Profiles: cruft inside TMPDIR containers (nix-shell.*, rc dirs, zephyr run
  dirs, /tmp/claude-{uid}, nested shells)
status: To Do
assignee: []
created_date: '2026-10-18 11:05'
updated_date: '2026-10-20 02:33'
labels:
  - wave-2
  - profiles
  - spike
dependencies: []
references:
  - docs/reference.md#built-in-profile-library
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: task-68's dry run on a real laptop (2026-10-18) found most /tmp cruft one or more levels down, inside directories that are some tool's TMPDIR, not at the top of /tmp: 4,184 of 4,185 eligible pytest-of-* dirs (10.4 GiB) lie in nix-shell.* or tmp.*; renode-<pid> dirs in zephyr-*-run.* and nix-shell.*; node-compile-cache and playwright* only in nix-shell.* (and nix-shell-*-* rc dirs); nested shells put pytest-of-* at depth 3 (nix-shell.X/nix-shell.Y/...); and Claude Code makes /tmp/claude-{uid} the TMPDIR of its tools, so 1,302 nix-shell.* dirs (and tmp*, loose files) live there, out of reach of nix-shell-tmpdir (root /tmp, depth 1). Without `**`, every profile repeats the container patterns (nix-shell.*/X, tmp.*/X, nix-shell-*-*/X, zephyr-*-run.*/X, claude-{uid}/X, nix-shell.*/nix-shell.*/X ...), and each new container family edits every profile (and, for built-ins, renames them).

Decision for Mark: (a) keep spelling containers out per profile (status quo; cheap, explicit, grows); (b) let a profile's roots name other profiles' matches (e.g. `roots: [/tmp, {profile: nix-shell-tmpdir}]`), so a container family is written once; (c) a fixed set of TMPDIR roots in config; (d) accept that the outer container profile (nix-shell-tmpdir, zephyr-tmp) is the unit of deletion and drop the inner rows for /tmp. Evidence and per-profile numbers: task-68 notes and crates/diskgc-core/src/fs_kind/spike-profiles.yaml.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The decision is recorded in docs/reference.md (Profiles or Built-in profile library) and the affected library rows follow it
- [ ] #2 nix-shell.* dirs below /tmp/claude-{uid} are either reachable by a /tmp profile or documented as out of scope
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decision by Mark (2026-10-18): a shared temp-dir prefix list (nix-shell.*, /tmp/claude-{uid}, zephyr run dirs, nested shells, ...), defined once and used by every /tmp profile, not repeated per profile: it avoids drift. Define its single home in docs/reference.md.

2026-10-20 implementation: built-in temp-prefix v1 is one finite shared list of 13 prefixes in profile/temp_roots.rs, with its documented order and a literal equality test. mktemp-dir, node-compile-cache, playwright-tmp and renode-tmp expand their base pattern through it; pytest-tmp is also implemented with the same preset but its full real-host dry run remains incomplete. The preset includes claude-{uid}/nix-shell.* and two shell levels, so entries under Claude Code TMPDIR are reachable. No recursive profile references. A changed prefix set requires a v2 preset and new profile name. nix-shell-tmpdir and broad tmp-entry remain planned pending dormant-session protection and full host evidence.

Final integrated nix develop -c just e2e passed 93/93 and QA/MPED reviews GO for the shared temp-prefix v1 preset. AC #2 remains open for the outer nix-shell.* directories below /tmp/claude-{uid}: the preset reaches inner profile matches there, but does not itself make those outer shell directories deletion candidates.

2026-10-20 release correction: nix-shell-rc is still unshipped. A private PID namespace can share host /tmp; real-kernel unshare proof found live NSpid 3025375 9 while host /proc/9/status was absent. Nix getpid() would encode 9 in the rc dirname, so a host-only proc_status(Pid(9)) returns Gone despite the live shell. Existing LinuxHost startup check covers only its own namespace. Need cross-namespace liveness proof and fresh pre-delete recheck before shipping rc. Outer nix-shell-tmpdir and tmp-entry also remain unshipped because dormant shells set TMPDIR after exec, absent from /proc environ/cwd/fds. TASK69 AC #2 remains open; no host deletion.
<!-- SECTION:NOTES:END -->
