---
id: TASK-336
title: Package README audit fixes and MIT license
status: In Progress
assignee: []
created_date: '2026-11-04T15:25:00Z'
updated_date: '2026-11-04T16:32:00Z'
labels:
  - docs
  - release
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-04: apply the readme-improver audit of packaging/README.md; license is MIT.

1. MIT LICENSE at the repo root (copyright holder: the repo's git author, Mark Ruvald Pedersen), license = "MIT" in [workspace.package] and every crate inheriting it, ship it in the release package as share/doc/diskgc/LICENSE through releases.json (files + every release row's ships list; release/tests.rs), and one License line in the README.
2. Root README.md for the repository (none today): one-liner and why, link to packaging/README.md for package users, dev quick start (nix develop -c just check, nix develop -c just e2e, nix build .#release-candidate), PRD.md and docs/ map, License. Short; no stats that rot.
3. packaging/README.md 'This package' list: add lib/systemd/{user,system}/diskgc-watch@.service, and that 0.2 runs them only with DISKGC_WATCH_ARGS=--tally-only (reference.md 'Watch as a systemd service').
4. packaging/README.md First run: one sentence that a large /tmp (tens of thousands of entries) needs --timeout above the default 10m (the advice sits above 'Known limits' in the release notes; dogfood 2026-11-04 hit exit 124 after 585 deletions).
5. packaging/README.md why: one sentence contrasting with age-only cleanup (find -mtime -delete, systemd-tmpfiles age), worded from docs/safety.md, no new claims.
6. packaging/README.md: a trimmed REAL human-format dry-run session from the release-candidate build (run it; never invent output).
7. The shipped RELEASE-NOTES.md carries internal TASK-n ids and the maintainer's name: decide with the release-notes cut (reference.md -> RELEASE-NOTES) whether to strip them at build time; keep reference.md as the single home.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-04 orchestrator: implementer in tmp/worktrees/t336 (branch t336-readme-mit, from master c9ec2c7e). License per Mark: MIT.
<!-- SECTION:NOTES:END -->
