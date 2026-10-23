---
id: TASK-123
title: >-
  Upstream Nix: give nix develop's NIX_BUILD_TOP temp dir owner evidence or exit
  cleanup
status: To Do
assignee: []
created_date: '2026-10-23 16:33'
labels:
  - upstream
  - nix
dependencies:
  - TASK-112
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: root cause of the nix-shell-tmpdir exclusion (TASK-112, docs/reference.md Liveness contracts). Nix 2.34.8's nix develop rc script runs mktemp -d -t nix-shell.XXXXXX for NIX_BUILD_TOP/TEMP/TMP/TEMPDIR after exec and installs no exit cleanup for it (legacy nix-shell has an EXIT trap; develop does not), so every develop shell leaks one directory (50k in /tmp on the development machine, 2026-10-23) and a live dormant shell's directory is indistinguishable from a dead one. Fix the class at the producer: propose upstream that the directory name carries the shell pid (e.g. nix-shell-<pid>.XXXXXX, as nix-develop-<pid>-<n> already does) and/or the rc installs an EXIT trap removing it. Then diskgc can admit it with the existing Owner PID contract (producer_gone pid_in_name) for Nix versions that have the fix. Do not file anything on GitHub without Mark's approval (repo rule: nothing leaves the machine without asking).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 An upstream proposal (issue or patch text) is drafted locally with a reproduction, and Mark has decided whether to send it
- [ ] #2 If merged upstream: a nix-shell-tmpdir-v? profile design names the minimum Nix version and its pid_in_name pattern
<!-- AC:END -->
